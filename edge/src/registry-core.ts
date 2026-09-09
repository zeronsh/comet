/**
 * Registry merge core — the pure row/op semantics behind RegistryRoom
 * (docs/registry-sync.md). No storage, no sockets: everything here is unit
 * tested in registry-core.test.ts and mirrored 1:1 by
 * crates/doc/src/registry.rs (shared test vectors — keep both in sync).
 *
 * The registry stores CURRENT STATE ONLY: a row is a bag of fields, each field
 * carries the HLC of its last write, and a write applies iff its clock beats
 * the stored one. History is discarded the moment it stops being true — the
 * property that makes the room wedge-proof (nothing grows, nothing replays).
 */

/** Hybrid-logical-clock string: `{ms:013}-{counter:06}-{device}`. Fixed-width
 * zero padding makes lexicographic order = (ms, counter, device) order, and
 * the device suffix makes it total — two distinct writers can never tie. */
export type Hlc = string;

export const encodeHlc = (ms: number, counter: number, device: string): Hlc =>
  `${String(ms).padStart(13, "0")}-${String(counter).padStart(6, "0")}-${device}`;

/** `a` strictly newer than `b` (undefined = never written, loses to any). */
export const hlcNewer = (a: Hlc, b: Hlc | undefined): boolean => b === undefined || a > b;

export type FieldValue = string | number | boolean | null | FieldValue[] | { [k: string]: FieldValue };

export interface Row {
  kind: string;
  id: string;
  /** Server seq of the batch that last touched this row (delta-sync key). */
  seq: number;
  deleted: boolean;
  /** Tombstone clock — an upsert newer than this revives the row. */
  delHlc?: Hlc;
  /** Lifecycle proof for the tombstone (RFC 0001 §9): the sealed record a
   * member authored for exactly this row and `delHlc`. Readers of an
   * encrypted generation accept a tombstone only with a verified proof;
   * cleared on revival. Opaque to the relay. */
  delProof?: FieldValue;
  fields: Record<string, FieldValue>;
  /** Per-field last-write clocks. */
  clocks: Record<string, Hlc>;
}

export type OpKind = "upsert" | "update" | "delete";

export interface Op {
  kind: string;
  id: string;
  op: OpKind;
  /** Field writes; `null` deletes the field (still a clocked write). Absent
   * for `delete` ops. */
  set?: Record<string, FieldValue | null>;
  /** Clock for every write in `set` without an entry in `clocks`. */
  hlc: Hlc;
  /** Per-field clock overrides — used by re-seed pushes to carry a row's
   * ORIGINAL clocks so recovery never coarsens causality. */
  clocks?: Record<string, Hlc>;
  /** Lifecycle proof for a `delete` op (stored as the tombstone's
   * `delProof`). Only meaningful on deletes; required in encrypted rooms. */
  proof?: FieldValue;
}

const ID_RE = /^[A-Za-z0-9_.:@\/-]{1,256}$/;
const KIND_RE = /^[a-z][a-zA-Z0-9]{0,31}$/;
const FIELD_RE = /^[a-zA-Z][a-zA-Z0-9]{0,63}$/;
const HLC_RE = /^\d{13}-\d{6}-[A-Za-z0-9_-]{1,128}$/;
/** Per-op serialized budget — a row is an index entry, never a document. */
export const MAX_OP_BYTES = 16 * 1024;

/** Structural validation for an op arriving off the wire. Returns an error
 * string or null. Merge assumes validated input. */
export const validateOp = (op: Op): string | null => {
  if (!KIND_RE.test(op.kind)) return "bad kind";
  if (!ID_RE.test(op.id)) return "bad id";
  if (op.op !== "upsert" && op.op !== "update" && op.op !== "delete") return "bad op";
  if (!HLC_RE.test(op.hlc)) return "bad hlc";
  if (op.proof !== undefined && (op.op !== "delete" || typeof op.proof !== "object" || op.proof === null)) {
    return "bad proof";
  }
  if (op.op === "delete") {
    if (op.set !== undefined) return "delete carries set";
  } else {
    if (op.set === undefined || typeof op.set !== "object" || Array.isArray(op.set)) {
      return "missing set";
    }
    for (const key of Object.keys(op.set)) {
      if (!FIELD_RE.test(key)) return `bad field: ${key}`;
    }
  }
  if (op.clocks !== undefined) {
    for (const [key, hlc] of Object.entries(op.clocks)) {
      if (!FIELD_RE.test(key)) return `bad clock field: ${key}`;
      if (!HLC_RE.test(hlc)) return `bad clock: ${key}`;
    }
  }
  if (JSON.stringify(op).length > MAX_OP_BYTES) return "op too large";
  return null;
};

export interface ApplyResult {
  /** Undefined only for an `update` op on a missing row (nothing to store). */
  row: Row | undefined;
  changed: boolean;
}

/**
 * Apply one op to a row (absent = undefined). Pure; returns the (possibly new)
 * row and whether anything changed. Callers stamp `row.seq` on change.
 *
 * Rules (docs/registry-sync.md):
 * - field write applies iff clock > stored clock for that field;
 * - `update` never creates or revives a row ("never invent rows");
 * - `upsert` creates, and revives a tombstone iff newer than `delHlc`;
 * - `delete` tombstones iff newer than `delHlc`, clearing fields/clocks;
 * - re-applying any op is a no-op (strict `>`), so re-pushes are idempotent.
 */
export const applyOp = (row: Row | undefined, op: Op): ApplyResult => {
  const clockFor = (field: string): Hlc => op.clocks?.[field] ?? op.hlc;

  if (op.op === "delete") {
    if (row === undefined) {
      // Tombstone-on-missing guards against a late create racing the delete.
      return {
        row: {
          kind: op.kind, id: op.id, seq: 0, deleted: true, delHlc: op.hlc,
          ...(op.proof !== undefined ? { delProof: op.proof } : {}),
          fields: {}, clocks: {}
        },
        changed: true
      };
    }
    if (row.deleted ? hlcNewer(op.hlc, row.delHlc) : hlcNewer(op.hlc, maxClock(row))) {
      // The proof travels with the tombstone it authorizes; an older proof
      // never lingers on a newer tombstone.
      const { delProof: _stale, ...rest } = row;
      return {
        row: {
          ...rest, deleted: true, delHlc: op.hlc,
          ...(op.proof !== undefined ? { delProof: op.proof } : {}),
          fields: {}, clocks: {}
        },
        changed: true
      };
    }
    return { row, changed: false };
  }

  let base: Row;
  if (row === undefined) {
    if (op.op === "update") return { row: undefined, changed: false };
    base = { kind: op.kind, id: op.id, seq: 0, deleted: false, fields: {}, clocks: {} };
  } else if (row.deleted) {
    if (op.op === "update") return { row, changed: false };
    if (!hlcNewer(op.hlc, row.delHlc)) return { row, changed: false };
    // Revival: the tombstone loses wholesale; the upsert's fields are the row.
    base = { kind: row.kind, id: row.id, seq: row.seq, deleted: false, delHlc: row.delHlc, fields: {}, clocks: {} };
  } else {
    base = row;
  }

  let changed = row === undefined || (row.deleted && !base.deleted);
  let fields = base.fields;
  let clocks = base.clocks;
  for (const [key, value] of Object.entries(op.set ?? {})) {
    const clock = clockFor(key);
    if (!hlcNewer(clock, clocks[key])) continue;
    if (fields === base.fields) {
      fields = { ...base.fields };
      clocks = { ...base.clocks };
    }
    if (value === null) delete fields[key];
    else fields[key] = value;
    clocks[key] = clock;
    changed = true;
  }
  if (!changed) return { row: base, changed: false };
  return { row: { ...base, deleted: false, fields, clocks }, changed: true };
};

/** The newest clock anywhere on a live row (delete-vs-live comparison base):
 * a delete only wins over a row it is causally newer than. */
export const maxClock = (row: Row): Hlc | undefined => {
  let max: Hlc | undefined = row.delHlc;
  for (const clock of Object.values(row.clocks)) {
    if (max === undefined || clock > max) max = clock;
  }
  return max;
};

/** A row as re-seed ops (server-behind-client recovery): one upsert carrying
 * the row's original per-field clocks, or a delete for tombstones. */
export const rowToSeedOp = (row: Row): Op => {
  if (row.deleted) {
    return {
      kind: row.kind, id: row.id, op: "delete", hlc: row.delHlc ?? encodeHlc(0, 0, "seed"),
      ...(row.delProof !== undefined ? { proof: row.delProof } : {})
    };
  }
  const max = maxClock(row) ?? encodeHlc(0, 0, "seed");
  return {
    kind: row.kind,
    id: row.id,
    op: "upsert",
    set: { ...row.fields },
    hlc: max,
    clocks: { ...row.clocks }
  };
};
