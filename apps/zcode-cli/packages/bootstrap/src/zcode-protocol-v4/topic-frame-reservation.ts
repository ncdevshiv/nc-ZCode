import type { TopicFrameDeliveryKind } from "@zcode/shared/zcode-protocol-v4";

/** Two-phase commit handle for publisher water level. */
export interface TopicFrameReservation<F> {
/** Assigned authoritatively by publisher admission, physical shards and consumers must not guess based on timing. */
  readonly deliveryKind: TopicFrameDeliveryKind;
  readonly logicalFrameId: string;
/** Monotonically increasing within the same subscription; used to reject PersistentProtocol old frame replay. */
  readonly logicalFrameOrdinal: number;
  readonly frame: F;
/** Only advance the water level while the current subscription generation is still valid. */
  commit(): boolean;
}
