// @ts-nocheck
import type { OrderTrace } from "./causality-order.js";
import type { AnalysisCore, CoreSites } from "./core.js";
import type { TaintOcc } from "./domain.js";

/**
* JSON encoding and decoding of `AnalysisCore`.
 *
* The core itself is pure data without position dependence. The only thing blocking `JSON.stringify` is `facts.*` and `types.*`
* in `Map` - they will be serialized into `{}`. Here, replace each Map with **ordered** `[key, value][]`:
* The array retains the insertion order, and is returned as is using `new Map(entries)` for decoding, so `serializeCore(decode(encode(c)))`
* Identical to `serializeCore(c)` byte-for-byte, the graph calculated when projected on the frozen artifact is consistent with the moment of export.
 *
* `sites` and `trace` only contain ordinary objects, arrays and optional fields, passed through as they are: `JSON.stringify` and the discarded values ​​are
* For `undefined` attributes, the decoder **doesn't** fill them in - projections all press "field is absent" instead of "field is"
* undefined" judgment, the two shapes are equivalent. The only exception is the value of `joinPortTypes`: `(string | undefined)[]`
* The `undefined` in the array will be written as `null` by JSON, so explicitly write `null` when encoding and change it back when decoding.
* `undefined`, so that "the port type is unknown" hole will still be `undefined` instead of `null` after the round trip.
 */

/** Ordered Map entries: Array order is the insertion order of Map. */
export type MapEntries<V> = [string, V][];

export interface AnalysisCoreJson {
/** Format version; the decoder only recognizes the version it recognizes, and throws the rest instead of guessing. */
  version: 1;
  sites: CoreSites;
  facts: {
    askData: MapEntries<TaintOcc[]>;
    askActor: MapEntries<TaintOcc[]>;
    worldReadData: MapEntries<TaintOcc[]>;
    joinIn: MapEntries<TaintOcc[]>;
    fanoutIn: MapEntries<TaintOcc[]>;
    returnData: TaintOcc[];
  };
  trace: OrderTrace;
  types: {
    siteType: MapEntries<string>;
/** The port type hole (`undefined`) is `null` here - there is no `undefined` in the JSON array. */
    joinPortTypes: MapEntries<(string | null)[]>;
  };
}

const VERSION = 1;

/** Turn core into a pure object that can be directly `JSON.stringify`; `sites` and `trace` share the original reference. */
export function encodeAnalysisCore(core: AnalysisCore): AnalysisCoreJson {
  return {
    facts: {
      askActor: [...core.facts.askActor],
      askData: [...core.facts.askData],
      fanoutIn: [...core.facts.fanoutIn],
      joinIn: [...core.facts.joinIn],
      returnData: core.facts.returnData,
      worldReadData: [...core.facts.worldReadData],
    },
    sites: core.sites,
    trace: core.trace,
    types: {
      joinPortTypes: [...core.types.joinPortTypes].map(([id, ports]) => [
        id,
        ports.map((port) => (port === undefined ? null : port)),
      ]),
      siteType: [...core.types.siteType],
    },
    version: VERSION,
  };
}

/** Rebuild core from JSON form (Map maintains original insertion order). Unknown versions throw a normal `Error`. */
export function decodeAnalysisCore(json: AnalysisCoreJson): AnalysisCore {
  if (json.version !== VERSION) {
    throw new Error(`unsupported AnalysisCoreJson version ${String(json.version)} (expected ${VERSION})`);
  }
  return {
    facts: {
      askActor: new Map(json.facts.askActor),
      askData: new Map(json.facts.askData),
      fanoutIn: new Map(json.facts.fanoutIn),
      joinIn: new Map(json.facts.joinIn),
      returnData: json.facts.returnData,
      worldReadData: new Map(json.facts.worldReadData),
    },
    sites: json.sites,
    trace: json.trace,
    types: {
      joinPortTypes: new Map(
        json.types.joinPortTypes.map(([id, ports]) => [id, ports.map((port) => (port === null ? undefined : port))]),
      ),
      siteType: new Map(json.types.siteType),
    },
  };
}
