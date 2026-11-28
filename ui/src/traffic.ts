import type { Data } from "./core.ts";

const counter = (v: unknown): number | null =>
  typeof v === "number" && Number.isSafeInteger(v) && v >= 0 ? v : null;
const tokenFields = [
  ["Uncached input", "uncachedInputTokens"],
  ["Output", "outputTokens"],
  ["Cache read", "cacheReadTokens"],
  ["Cache write", "cacheWriteTokens"],
];
export const isFailure = (e: Data) =>
  !["success", "pending", "client_cancelled", "fallback_served"].includes(
    e.outcome,
  );
const outcomes: Record<string, string> = {
  success: "Succeeded",
  client_cancelled: "Cancelled by client",
  pending: "No final outcome recorded",
  overloaded_error: "Provider overloaded",
  rate_limit_error: "Provider rate limit reached",
  quota_unavailable: "No account has available quota",
  authentication_error: "Account sign-in rejected",
  transport_lost: "Connection lost",
  incomplete_stream: "Response interrupted",
  observation_incomplete: "Response observation incomplete",
  attempt_limit: "Account retry limit reached",
  provider_error: "Provider error",
  stream_error: "Response stream error",
  upstream_stream_error: "Response stream error",
  api_error: "Provider API error",
  invalid_request_error: "Request rejected as invalid",
  not_found_error: "Not found",
  permission_error: "Permission denied",
};
const fallbackFailures: Record<string, string> = {
  unknown_model: "no provider serves it",
  provider_not_allowed: "its provider cannot serve a fallback",
  no_output: "it gave no answer",
};
/** Why a rejected request's quota fallback did not answer. */
export const fallbackNote = (e: Data) =>
  e.fallbackFailure && e.fallbackModel
    ? `Fallback to ${e.fallbackModel} failed: ${fallbackFailures[e.fallbackFailure] || e.fallbackFailure}`
    : undefined;
export const outcomeLabel = (e: Data) =>
  e.recovered
    ? "Recovered after account switch"
    : e.outcome === "fallback_served"
      ? `Fell back to ${e.fallbackModel || "another model"}`
      : (e.outcome === "failed" || e.outcome === "failover") &&
          counter(e.status) !== null
        ? `HTTP ${e.status}`
        : Object.hasOwn(outcomes, e.outcome)
        ? outcomes[e.outcome]
        : String(e.outcome || "Unknown outcome").replaceAll("_", " ");

/** Request totals from the buckets, and token totals from the exact sends. */
export function summarizeHistory(history: Data) {
  const buckets = Array.isArray(history.buckets)
    ? (history.buckets as Data[])
    : [];
  const models = Array.isArray(history.models)
    ? (history.models as Data[])
    : [];
  const requests = buckets.reduce(
    (n, bucket) => n + (counter(bucket.requests) ?? 0),
    0,
  );
  const failures = buckets.reduce(
    (n, bucket) => n + (counter(bucket.failures) ?? 0),
    0,
  );
  const totals: Data = history.tokens || {};
  const sends = counter(totals.sends) ?? 0;
  const exactSends = counter(totals.exactSends) ?? 0;
  const tokens = tokenFields.map(([label, field]) => ({
    label,
    tokens: exactSends > 0 ? counter(totals[field]) : null,
  }));
  return {
    buckets,
    models,
    requests,
    failures,
    tokens,
    partialTokens: exactSends < sends,
  };
}
