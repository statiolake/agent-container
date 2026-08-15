#!/usr/bin/env node

import http from "node:http";
import { randomUUID } from "node:crypto";

const ARGUMENTS_HEADER = "x-agent-container-task-arguments";
const REQUEST_ID_HEADER = "x-agent-container-task-request-id";
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
      "stdin is streamed only for tasks with allow_stdin enabled, and",
      "stdout/stderr are streamed back to the corresponding local streams.",
      "For example:",
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

function openRequest(target, method, headers = {}) {
  const proxy = proxyFromEnvironment(target);
  const requestTarget = proxy
    ? target.href
    : target.pathname + target.search;
  const options = {
    protocol: proxy?.protocol || target.protocol,
    hostname: proxy?.hostname || target.hostname,
    port: proxy?.port || target.port || undefined,
    method,
    path: requestTarget,
    headers: {
      Host: target.host,
      ...headers,
      Connection: "close",
    },
  };
  return http.request(options);
}

function openTaskRequest(target, encodedArguments, requestId) {
  return openRequest(target, "POST", {
    [ARGUMENTS_HEADER]: encodedArguments,
    [REQUEST_ID_HEADER]: requestId,
    "Content-Type": "application/octet-stream",
    "Transfer-Encoding": "chunked",
    Accept: STREAM_CONTENT_TYPE,
  });
}

function describeRequestError(error) {
  const message = error?.message || String(error);
  const details = [error?.code, error?.syscall]
    .filter(Boolean)
    .join(", ");
  return details ? `${message} (${details})` : message;
}

function fetchTaskInfo(target, requestId) {
  return new Promise((resolve, reject) => {
    const request = openRequest(target, "GET", {
      Accept: "application/json",
      [REQUEST_ID_HEADER]: requestId,
    });
    let body = "";
    request.on("response", (response) => {
      response.setEncoding("utf8");
      response.on("data", (chunk) => {
        body += chunk;
      });
      response.on("end", () => {
        if (response.statusCode !== 200) {
          reject(
            new Error(
              "broker rejected the task (" +
                response.statusCode +
                "): " +
                body.trim(),
            ),
          );
          return;
        }
        let info;
        try {
          info = JSON.parse(body);
        } catch (error) {
          reject(new Error("broker returned invalid task metadata: " + error.message));
          return;
        }
        if (typeof info.allow_stdin !== "boolean") {
          reject(new Error("broker returned invalid task metadata: allow_stdin is missing"));
          return;
        }
        resolve(info);
      });
    });
    request.on("error", reject);
    request.end();
  });
}

function handleStreamResponse(response, requestId) {
  let responseEnded = false;
  let responseFailureReported = false;

  const reportResponseFailure = (message) => {
    if (responseEnded || responseFailureReported) {
      return;
    }
    responseFailureReported = true;
    fail(message, 1);
  };

  response.on("aborted", () => {
    reportResponseFailure(
      `broker stream response aborted (request_id=${requestId})`,
    );
  });
  response.on("error", (error) => {
    reportResponseFailure(
      `broker stream response error (request_id=${requestId}): ${describeRequestError(error)}`,
    );
  });
  response.on("close", () => {
    reportResponseFailure(
      `broker stream response closed before end (request_id=${requestId})`,
    );
  });

  if (response.statusCode !== 200) {
    let body = "";
    response.setEncoding("utf8");
    response.on("data", (chunk) => {
      body += chunk;
    });
    response.on("end", () => {
      responseEnded = true;
      fail(
        "broker rejected the task (" +
          response.statusCode +
          "): " +
          body.trim() +
          ` (request_id=${requestId})`,
        1,
      );
    });
    return;
  }

  const contentType = response.headers["content-type"] || "";
  if (!contentType.startsWith(STREAM_CONTENT_TYPE)) {
    fail(
      `broker returned an unexpected content type: ${contentType} (request_id=${requestId})`,
      1,
    );
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
    } else if (frame.type === "keepalive") {
      // Keep the HTTP stream active through idle-timeout proxies.
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
    responseEnded = true;
    if (pending.trim()) {
      consumeLine(pending);
    }
    if (!exitFrame) {
      fail(
        responseError
          ? `${responseError} (request_id=${requestId})`
          : `broker closed the task stream without an exit frame (request_id=${requestId})`,
        1,
      );
      return;
    }
    process.exitCode =
      typeof exitFrame.code === "number" ? exitFrame.code : 1;
  });
}

async function run(parsed) {
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
      const requestId = randomUUID();

      let taskInfo;
      try {
        taskInfo = await fetchTaskInfo(target, requestId);
      } catch (error) {
        fail(
          `broker metadata GET failed (request_id=${requestId}): ${describeRequestError(error)}`,
          1,
        );
        return;
      }

      const request = openTaskRequest(target, encodedArguments, requestId);
      request.on("response", (response) =>
        handleStreamResponse(response, requestId),
      );
      request.on("error", (error) => {
        fail(
          `broker stream POST failed (request_id=${requestId}): ${describeRequestError(error)}`,
          1,
        );
      });

      if (!taskInfo.allow_stdin || process.stdin.isTTY) {
        request.end();
      } else {
        process.stdin.pipe(request);
      }
    }
  }
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
  run(parsed).catch((error) => fail(error.message, 1));
}
