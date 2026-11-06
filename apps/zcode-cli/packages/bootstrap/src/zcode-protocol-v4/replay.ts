// Playback export bucket.
//
// The downstream playback page uses the CLI's own three-segment reducer in the browser to reconstruct the persistent MessageWithParts into
// ConversationSnapshot：synthesizeEventsFromMessages → mergeColdConversationEvents →
// ProductProjection(hydration replay). All three are pure functions/pure classes, without node dependencies - this discipline is governed by
// `vite build` mechanical check of the browser playback package (once the node built-in module import is mixed into the bucket, the browser package will immediately fail to build).
//
// Just re-export, don't define anything.
export { ProductProjection } from "./product-projection.js";
export { synthesizeEventsFromMessages } from "./transcript-hydration.js";
export { mergeColdConversationEvents, type ColdEventMergeResult } from "./cold-event-merge.js";
export { HYDRATION_TRACE_ID } from "./projection-state.js";
