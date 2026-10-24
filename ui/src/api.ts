import { Lifetime } from "./lifetime.ts";
import type { Data } from "./core";

// The management key lives only in this module's memory. It is never written to storage.
// Requests go only to the server that served this page: the API sits next to it
// (/management.html -> /v8/management, also under a reverse-proxy path prefix).
export const base = new URL(".", location.href).href.replace(/\/$/, "");
let key = "";
let local = false;
export let requests = new Lifetime();
let hooks: {
  unauthorized(): void;
  missing(method: string, path: string): void;
  meta(h: Headers | null): void;
} = { unauthorized() {}, missing() {}, meta() {} };

export class ApiError extends Error {
  status: number;
  code: string;
  method: string;
  path: string;
  constructor(
    message: string,
    status: number,
    code: string,
    method: string,
    path: string,
  ) {
    super(message);
    this.status = status;
    this.code = code;
    this.method = method;
    this.path = path;
  }
  /** The server does not implement this route or method (Rust 501/405, Go's empty 404). */
  get missing() {
    return this.status === 501 || this.status === 405 || (this.status === 404 && !this.code);
  }
}

export function connect(secret: string, h: typeof hooks, localDashboard = false) {
  key = secret;
  local = localDashboard;
  hooks = h;
  requests.dispose();
  requests = new Lifetime();
}
export function disconnect() {
  key = "";
  local = false;
  requests.dispose();
  requests = new Lifetime();
}
export const endpoint = () => `${base}/v8/management`;
export const route = (path: string) => path.split("?")[0];

async function send(path: string, method: string, body?: unknown) {
  if (!key && !local) throw new Error("Sign in with your management key.");
  const lifetime = requests;
  const headers: Record<string, string> = local
    ? { "X-CPA-Local-Dashboard": "1" }
    : { Authorization: `Bearer ${key}` };
  const yaml = path === "/config.yaml";
  if (body !== undefined && !(body instanceof FormData))
    headers["Content-Type"] = yaml ? "application/yaml" : "application/json";
  let response: Response;
  try {
    response = await fetch(endpoint() + path, {
      method,
      headers,
      body: body === undefined || body instanceof FormData || yaml ? (body as BodyInit) : JSON.stringify(body),
      signal: AbortSignal.any([lifetime.signal, AbortSignal.timeout(path.includes("/refresh") ? 300_000 : 30_000)]),
      cache: "no-store",
      redirect: "error",
    });
  } catch (e) {
    if (lifetime.active) hooks.meta(null);
    throw new Error(
      !lifetime.active
        ? "Connection changed."
        : e instanceof Error && e.name === "TimeoutError"
          ? "The server timed out."
          : "Cannot reach cliproxy. Check that it is running.",
    );
  }
  if (!lifetime.active) throw new Error("Connection changed.");
  hooks.meta(response.headers);
  return response;
}

export async function api(
  path: string,
  method = "GET",
  body?: unknown,
  format: "json" | "text" | "blob" = "json",
): Promise<any> {
  const lifetime = requests;
  const response = await send(path, method, body);
  const ok = response.ok;
  const value = !ok
    ? await response.json().catch(() => ({}))
    : format === "blob"
      ? await response.blob()
      : format === "text"
        ? await response.text()
        : response.status === 204
          ? {}
          : await response.json();
  // A body that finishes after sign-out or a reconnect belongs to the old session.
  if (!lifetime.active) throw new Error("Connection changed.");
  if (ok) return value;
  if (response.status === 401) hooks.unauthorized();
  const error = new ApiError(
    String(value.message || value.error || `HTTP ${response.status}`),
    response.status,
    String(value.code || value.error || ""),
    method,
    route(path),
  );
  if (error.missing) {
    error.message = `Not available on this server (${method} ${route(path)} → ${response.status}).`;
    hooks.missing(method, route(path));
  }
  throw error;
}
/** True when the server lacks the route; other errors are real failures. */
export const missing = (e: unknown) => e instanceof ApiError && e.missing;
export const text = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Asks this proxy for its model list with a client key, as a tool would. No upstream call. */
export async function clientModels(clientKey: string): Promise<string[]> {
  const r = await fetch(`${base}/v1/models`, { headers: { Authorization: `Bearer ${clientKey}` }, cache: "no-store" });
  if (r.status === 401) throw new Error("The proxy rejected this key. Check that it is listed under Client keys.");
  if (!r.ok) throw new Error(`The proxy answered HTTP ${r.status}. Check Logs for the reason.`);
  return ((await r.json()).data || []).map((m: Data) => String(m.id));
}

export async function configValue(path: string, fallback: unknown) {
  try {
    return await api(path);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404 && e.code === "not_found") return fallback;
    throw e;
  }
}

export function download(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob),
    a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
