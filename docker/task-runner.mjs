#!/usr/bin/env node

import http from "node:http";

const ARGUMENTS_HEADER = "x-agent-container-task-arguments";
const STREAM_CONTENT_TYPE =
  "application/x-agent-container-task-runner-stream; version=1";

function usage() {
  process.stderr.write(
    [
      "Usage: task-runner TASK [KEY=VALUE ...] [-- ARG ...]",
      "",
      "Run a task exposed by the task-runner MCP server through the host broker.",
      "The MCP server is authoritative for available task names; this command",
      "cannot execute arbitrary commands or define new tasks.",
      "",
      "  KEY=VALUE          expose a named task argument as an environment variable",
      "  -- ARG ...         pass ordered positional arguments to the task's \"$@\"",
      "  --timeout-seconds N",
      "                     set the same timeout used by the task-runner MCP tool",
      "",
      "stdin is streamed to the host task, and stdout/stderr are streamed back",
      "to the corresponding local streams. For example:",
      "",
      "  task-runner deploy environment=staging",
      "  printf '%s\\n' \"$PR_BODY\" | task-runner review -- --format markdown",
      "",
    ].join("\n"),
  );
}

function fail(message, code = 64) {
  process.stderr.write("task-runner: " + message + "\n");
  process.exitCode = code;
}

function parseArguments(rawArgs) {
  const [task, ...rest] = rawArgs;
  if (!task || task === "--help" || task === "-h") {
    usage();
    return null;
  }
  return { task, arguments: rest };
}

function proxyFromEnvironment(target) {
  const proxyValue = process.env.HTTP_PROXY || process.env.http_proxy;
  if (!proxyValue) {
    return null;
  }
  const proxy = new URL(proxyValue);
  const noProxy = (process.env.NO_PROXY || process.env.no_proxy || "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  if (
    noProxy.some(
      (value) =>
        value === "*" ||
        value === target.hostname ||
        value === target.host ||
        (value.startsWith(".") && target.hostname.endsWith(value)),
    )
  ) {
    return null;
  }
  return proxy;
}

function openRequest(target, encodedArguments) {
  const proxy = proxyFromEnvironment(target);
  const requestTarget = proxy
    ? target.href
    : target.pathname + target.search;
  const options = {
    protocol: proxy?.protocol || target.protocol,
    hostname: proxy?.hostname || target.hostname,
    port: proxy?.port || target.port || undefined,
    method: "POST",
    path: requestTarget,
    headers: {
      Host: target.host,
      [ARGUMENTS_HEADER]: encodedArguments,
      "Content-Type": "application/octet-stream",
      "Transfer-Encoding": "chunked",
      Accept: STREAM_CONTENT_TYPE,
      Connection: "close",
    },
  };
  return http.request(options);
}

function handleStreamResponse(response) {
  if (response.statusCode !== 200) {
    let body = "";
    response.setEncoding("utf8");
    response.on("data", (chunk) => {
      body += chunk;
    });
    response.on("end", () => {
      fail(
        "broker rejected the task (" +
          response.statusCode +
          "): " +
          body.trim(),
        1,
      );
    });
    return;
  }

  const contentType = response.headers["content-type"] || "";
  if (!contentType.startsWith(STREAM_CONTENT_TYPE)) {
    fail("broker returned an unexpected content type: " + contentType, 1);
    response.resume();
    return;
  }

  let pending = "";
  let exitFrame = null;
  let responseError = null;
  response.setEncoding("utf8");

  const consumeLine = (line) => {
    if (!line) {
      return;
    }
    let frame;
    try {
      frame = JSON.parse(line);
    } catch (error) {
      responseError = "invalid broker stream frame: " + error.message;
      return;
    }
    if (frame.type === "data") {
      const output = Buffer.from(frame.data || "", "base64");
      const destination =
        frame.stream === "stderr" ? process.stderr : process.stdout;
      if (!destination.write(output)) {
        response.pause();
        destination.once("drain", () => response.resume());
      }
    } else if (frame.type === "error") {
      process.stderr.write(
        "task-runner: " + (frame.message || "task failed") + "\n",
      );
      responseError = frame.message || "task failed";
    } else if (frame.type === "exit") {
      exitFrame = frame;
    }
  };

  response.on("data", (chunk) => {
    pending += chunk;
    let newline;
    while ((newline = pending.indexOf("\n")) !== -1) {
      consumeLine(pending.slice(0, newline));
      pending = pending.slice(newline + 1);
    }
  });
  response.on("end", () => {
    if (pending.trim()) {
      consumeLine(pending);
    }
    if (!exitFrame) {
      fail(
        responseError ||
          "broker closed the task stream without an exit frame",
        1,
      );
      return;
    }
    process.exitCode =
      typeof exitFrame.code === "number" ? exitFrame.code : 1;
  });
}

let parsed;
try {
  parsed = parseArguments(process.argv.slice(2));
} catch (error) {
  fail(error.message);
  parsed = null;
}

if (!parsed) {
  if (process.argv.length <= 2) {
    process.exitCode = 64;
  }
} else {
  const endpoint = process.env.AGENT_CONTAINER_HOST_ENDPOINT;
  if (!endpoint) {
    fail("AGENT_CONTAINER_HOST_ENDPOINT is not set; run inside agent-container");
  } else {
    let target;
    try {
      target = new URL(
        endpoint.replace(/\/+$/, "") +
          "/task-runner/" +
          encodeURIComponent(parsed.task),
      );
      if (target.protocol !== "http:") {
        throw new Error("unsupported broker URL scheme " + target.protocol);
      }
    } catch (error) {
      fail("invalid broker endpoint: " + error.message);
      target = null;
    }

    if (target) {
      const encodedArguments = Buffer.from(
        JSON.stringify(parsed.arguments),
      ).toString("base64url");
      const request = openRequest(target, encodedArguments);
      request.on("response", handleStreamResponse);
      request.on("error", (error) => {
        fail("broker request failed: " + error.message, 1);
      });

      if (process.stdin.isTTY) {
        request.end();
      } else {
        process.stdin.pipe(request);
      }
    }
  }
}
