import { z } from "zod";

export const PERMISSION_FULL_ACCESS_ENTRY = "runtime/permission_full_access";

/** The persistent receipt is the source of fact for authorization retry; it refuses to retry when it is damaged, and the authorization scope cannot be re-expanded according to the current queue. */
export const permissionFullAccessReceiptSchema = z
  .object({
    interactionId: z.string().min(1),
    event: z
      .object({
        id: z.string().min(1),
        sessionId: z.string().min(1),
        traceId: z.string().min(1),
        turnId: z.string().optional(),
        type: z.literal("session_mode_changed"),
        timestamp: z.coerce.date(),
        sequenceNumber: z.number().int().nonnegative(),
        payload: z
          .object({
            mode: z.literal("yolo"),
            planEnabled: z.boolean(),
            previousMode: z.enum(["build", "edit", "yolo", "auto", "plan"]),
            previousPlanEnabled: z.boolean(),
            source: z.literal("command"),
            permissionGrant: z
              .object({
                interactionId: z.string().min(1),
                queueItemIds: z.array(z.string().min(1)),
              })
              .strict(),
          })
          .strict(),
      })
      .strict(),
  })
  .strict()
  .refine(
    (receipt) => receipt.interactionId === receipt.event.payload.permissionGrant.interactionId,
    {
      message: "Permission grant identity mismatch",
    },
  );
