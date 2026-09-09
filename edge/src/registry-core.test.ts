import { describe, expect, it } from "vitest";
import {
  applyOp,
  encodeHlc,
  hlcNewer,
  maxClock,
  rowToSeedOp,
  validateOp,
  type Op,
  type Row
} from "./registry-core";

const hlc = (ms: number, device = "dev-a", counter = 0) => encodeHlc(ms, counter, device);

const upsert = (over: Partial<Op> = {}): Op => ({
  kind: "chats",
  id: "chat-1",
  op: "upsert",
  set: { title: "hello", archived: false },
  hlc: hlc(1000),
  ...over
});

const del = (at: string, proof?: Op["proof"]): Op => ({
  kind: "chats",
  id: "chat-1",
  op: "delete",
  hlc: at,
  ...(proof !== undefined ? { proof } : {})
});

const applied = (row: Row | undefined, op: Op): Row => {
  const result = applyOp(row, op);
  expect(result.changed).toBe(true);
  expect(result.row).toBeDefined();
  return result.row as Row;
};

describe("hlc", () => {
  it("orders by (ms, counter, device) lexicographically", () => {
    expect(hlc(2) > hlc(1)).toBe(true);
    expect(encodeHlc(1, 2, "a") > encodeHlc(1, 1, "a")).toBe(true);
    expect(encodeHlc(1, 1, "b") > encodeHlc(1, 1, "a")).toBe(true);
    // Fixed width: a 4-digit ms never compares below a 1-digit one.
    expect(hlc(10000) > hlc(999)).toBe(true);
    expect(hlcNewer(hlc(1), undefined)).toBe(true);
    expect(hlcNewer(hlc(1), hlc(1))).toBe(false);
  });
});

describe("applyOp", () => {
  it("creates rows via upsert, never via update", () => {
    const row = applied(undefined, upsert());
    expect(row.fields).toEqual({ title: "hello", archived: false });
    expect(row.clocks.title).toBe(hlc(1000));
    expect(row.deleted).toBe(false);

    const miss = applyOp(undefined, { ...upsert(), op: "update" });
    expect(miss.changed).toBe(false);
    expect(miss.row).toBeUndefined();
  });

  it("field-level LWW: newer clock wins, older loses, ties lose", () => {
    let row = applied(undefined, upsert());
    // Older write on one field: ignored, other fields untouched.
    const older = applyOp(row, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { title: "stale" },
      hlc: hlc(500)
    });
    expect(older.changed).toBe(false);
    // Equal clock (same device, same instant): strict > means no-op — the
    // property that makes reconnect re-pushes idempotent.
    const replay = applyOp(row, upsert());
    expect(replay.changed).toBe(false);
    // Newer write on one field leaves the other's clock alone.
    row = applied(row, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { title: "renamed" },
      hlc: hlc(2000, "dev-b")
    });
    expect(row.fields).toEqual({ title: "renamed", archived: false });
    expect(row.clocks.archived).toBe(hlc(1000));
  });

  it("same-ms conflicts settle by device id, deterministically", () => {
    const base = applied(undefined, upsert());
    const fromA: Op = { kind: "chats", id: "chat-1", op: "update", set: { title: "A" }, hlc: hlc(5000, "dev-a") };
    const fromB: Op = { kind: "chats", id: "chat-1", op: "update", set: { title: "B" }, hlc: hlc(5000, "dev-b") };
    const ab = applyOp(applyOp(base, fromA).row, fromB);
    const ba = applyOp(applyOp(base, fromB).row, fromA);
    // Same winner regardless of arrival order (dev-b > dev-a).
    expect((ab.row as Row).fields.title).toBe("B");
    expect((ba.row as Row).fields.title).toBe("B");
  });

  it("null field values delete the field (still clocked)", () => {
    let row = applied(undefined, upsert({ set: { title: "x", name: "y" } }));
    row = applied(row, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { name: null },
      hlc: hlc(2000)
    });
    expect(row.fields).toEqual({ title: "x" });
    expect(row.clocks.name).toBe(hlc(2000));
    // A write older than the deletion cannot resurrect the field.
    const stale = applyOp(row, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { name: "zombie" },
      hlc: hlc(1500)
    });
    expect(stale.changed).toBe(false);
  });

  it("delete tombstones only when causally newer than the row", () => {
    const row = applied(undefined, upsert({ hlc: hlc(1000) }));
    // A delete older than the newest field write loses wholesale.
    const staleDelete = applyOp(row, { kind: "chats", id: "chat-1", op: "delete", hlc: hlc(500) });
    expect(staleDelete.changed).toBe(false);
    // A newer delete wins and clears fields.
    const gone = applied(row, { kind: "chats", id: "chat-1", op: "delete", hlc: hlc(2000) });
    expect(gone.deleted).toBe(true);
    expect(gone.delHlc).toBe(hlc(2000));
    expect(gone.fields).toEqual({});
    // Updates never touch a tombstone.
    const update = applyOp(gone, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { title: "ghost" },
      hlc: hlc(3000)
    });
    expect(update.changed).toBe(false);
    // An older upsert cannot revive it…
    const staleRevive = applyOp(gone, upsert({ hlc: hlc(1500) }));
    expect(staleRevive.changed).toBe(false);
    // …a newer one can, and starts from ONLY its own fields.
    const revived = applied(gone, upsert({ set: { title: "back" }, hlc: hlc(4000) }));
    expect(revived.deleted).toBe(false);
    expect(revived.fields).toEqual({ title: "back" });
  });

  it("delete on a missing row plants a guard tombstone", () => {
    const gone = applied(undefined, { kind: "chats", id: "chat-9", op: "delete", hlc: hlc(1000) });
    expect(gone.deleted).toBe(true);
    // The guard blocks the late create it exists for.
    const late = applyOp(gone, upsert({ id: "chat-9", hlc: hlc(500) }));
    expect(late.changed).toBe(false);
  });

  it("per-field clock overrides (re-seed) preserve original causality", () => {
    let row = applied(undefined, upsert({ set: { title: "old", status: "idle" } }));
    row = applied(row, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { status: "working" },
      hlc: hlc(9000)
    });
    // Re-seed the row elsewhere from its seed op…
    const seeded = applied(undefined, rowToSeedOp(row));
    expect(seeded.fields).toEqual(row.fields);
    expect(seeded.clocks).toEqual(row.clocks);
    // …and a mid-history write STILL loses against the preserved clocks.
    const stale = applyOp(seeded, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { status: "errored" },
      hlc: hlc(5000)
    });
    expect(stale.changed).toBe(false);
  });

  it("carries the lifecycle proof on the tombstone and clears it on revival", () => {
    const proof = { e1: "c2VhbGVk" };
    const gone = applied(applied(undefined, upsert()), del(hlc(2000), proof));
    expect(gone.deleted).toBe(true);
    expect(gone.delProof).toEqual(proof);
    // A newer delete replaces the proof; an older one leaves it untouched.
    const again = applied(gone, del(hlc(3000), { e1: "bmV3ZXI=" }));
    expect(again.delProof).toEqual({ e1: "bmV3ZXI=" });
    expect(applyOp(again, del(hlc(2500), proof)).changed).toBe(false);
    // Re-seeding a tombstone carries its proof back as the op's `proof`.
    expect(rowToSeedOp(again)).toEqual({ kind: "chats", id: "chat-1", op: "delete", hlc: hlc(3000), proof: { e1: "bmV3ZXI=" } });
    // Revival drops the proof with the tombstone it authorized.
    const revived = applied(again, upsert({ hlc: hlc(4000) }));
    expect(revived.deleted).toBe(false);
    expect(revived.delProof).toBeUndefined();
    expect(revived.delHlc).toBe(hlc(3000));
  });

  it("rowToSeedOp round-trips tombstones", () => {
    const gone = applied(undefined, { kind: "spaces", id: "sp-1", op: "delete", hlc: hlc(7000) });
    const seeded = applied(undefined, rowToSeedOp(gone));
    expect(seeded.deleted).toBe(true);
    expect(seeded.delHlc).toBe(hlc(7000));
  });

  it("converges over every causally-valid arrival order", () => {
    // The server sees each device's ops in push order, and a device only
    // updates rows it has seen — so the row's create always precedes the
    // updates. Permute everything AFTER the create (cross-device races).
    const create = upsert({ hlc: hlc(1000) });
    const races: Op[] = [
      { kind: "chats", id: "chat-1", op: "update", set: { title: "renamed" }, hlc: hlc(3000, "dev-b") },
      { kind: "chats", id: "chat-1", op: "update", set: { archived: true }, hlc: hlc(2000, "dev-c") },
      { kind: "chats", id: "chat-1", op: "upsert", set: { title: "other", cwd: "/tmp" }, hlc: hlc(2500, "dev-d") }
    ];
    const outcomes: Row[] = [];
    const permute = (rest: Op[], acc: Op[]) => {
      if (rest.length === 0) {
        let row: Row | undefined;
        for (const op of [create, ...acc]) row = applyOp(row, op).row ?? row;
        outcomes.push(row as Row);
        return;
      }
      for (let i = 0; i < rest.length; i++) {
        permute([...rest.slice(0, i), ...rest.slice(i + 1)], [...acc, rest[i]]);
      }
    };
    permute(races, []);
    expect(outcomes).toHaveLength(6);
    for (const outcome of outcomes) {
      expect(outcome.fields).toEqual(outcomes[0].fields);
      expect(outcome.clocks).toEqual(outcomes[0].clocks);
      expect(outcome.deleted).toBe(false);
    }
    expect(outcomes[0].fields.title).toBe("renamed");
    expect(outcomes[0].fields.archived).toBe(true);
    expect(outcomes[0].fields.cwd).toBe("/tmp");
  });

  it("drops an update that outruns its row's create (never invent rows)", () => {
    // Causally impossible on the wire (per-device batches are ordered), but a
    // hand-crafted client could send it: the update is dropped, not deferred.
    const early = applyOp(undefined, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { title: "too early" },
      hlc: hlc(3000)
    });
    expect(early.changed).toBe(false);
    const row = applied(undefined, upsert({ hlc: hlc(1000) }));
    expect(row.fields.title).toBe("hello");
  });
});

describe("validateOp", () => {
  it("allows a proof only on delete ops, and only as an object", () => {
    expect(validateOp({ ...upsert(), proof: { e1: "x" } })).toBe("bad proof");
    expect(validateOp(del(hlc(1), "nope" as unknown as Op["proof"]))).toBe("bad proof");
    expect(validateOp(del(hlc(1), { e1: "x" }))).toBeNull();
    expect(validateOp(del(hlc(1)))).toBeNull();
  });

  it("accepts well-formed ops and rejects malformed ones", () => {
    expect(validateOp(upsert())).toBeNull();
    expect(validateOp({ ...upsert(), kind: "Nope Kind" })).toMatch(/kind/);
    expect(validateOp({ ...upsert(), id: "" })).toMatch(/id/);
    expect(validateOp({ ...upsert(), op: "merge" as unknown as Op["op"] })).toMatch(/op/);
    expect(validateOp({ ...upsert(), hlc: "not-a-clock" })).toMatch(/hlc/);
    expect(validateOp({ ...upsert(), set: undefined })).toMatch(/set/);
    expect(validateOp({ ...upsert(), set: { "bad field!": 1 } })).toMatch(/field/);
    expect(validateOp({ kind: "chats", id: "c", op: "delete", hlc: hlc(1), set: {} })).toMatch(/delete/);
    expect(validateOp({ ...upsert(), clocks: { title: "junk" } })).toMatch(/clock/);
    const huge = upsert({ set: { blob: "x".repeat(20_000) } });
    expect(validateOp(huge)).toMatch(/large/);
  });
});

describe("maxClock", () => {
  it("returns the newest clock across fields and tombstone", () => {
    const row = applied(undefined, upsert({ set: { a: 1, b: 2 }, hlc: hlc(1000) }));
    const bumped = applied(row, {
      kind: "chats",
      id: "chat-1",
      op: "update",
      set: { b: 3 },
      hlc: hlc(5000)
    });
    expect(maxClock(bumped)).toBe(hlc(5000));
    expect(maxClock({ kind: "x", id: "y", seq: 0, deleted: true, delHlc: hlc(9), fields: {}, clocks: {} })).toBe(hlc(9));
  });
});
