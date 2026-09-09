// Registry merge conformance vectors — a 1:1 port of
// edge/src/registry-core.test.ts (same inputs, same expected rows). These are
// the cross-language vectors shared with registry-core.ts (vitest) and
// crates/doc/src/registry.rs (cargo test); change all three together.

import XCTest
@testable import Zeron

private func hlc(_ ms: Int64, _ device: String = "dev-a", _ counter: UInt32 = 0) -> Hlc {
    encodeHlc(ms: ms, counter: counter, device: device)
}

private func upsert(id: String = "chat-1",
                    set: [String: JSONValue] = ["title": .string("hello"), "archived": .bool(false)],
                    hlc at: Hlc = hlc(1000),
                    clocks: [String: Hlc]? = nil) -> RegistryOp {
    RegistryOp(kind: "chats", id: id, op: .upsert, set: set, hlc: at, clocks: clocks)
}

private func update(id: String = "chat-1", set: [String: JSONValue], hlc at: Hlc) -> RegistryOp {
    RegistryOp(kind: "chats", id: id, op: .update, set: set, hlc: at, clocks: nil)
}

private func deleteOp(kind: String = "chats", id: String = "chat-1", hlc at: Hlc) -> RegistryOp {
    RegistryOp(kind: kind, id: id, op: .delete, set: nil, hlc: at, clocks: nil)
}

/// applyOp asserting the op changed something (the test.ts `applied` helper).
private func applied(_ row: RegistryRow?, _ op: RegistryOp,
                     file: StaticString = #filePath, line: UInt = #line) -> RegistryRow {
    let result = applyOp(row, op)
    XCTAssertTrue(result.changed, "expected op to apply", file: file, line: line)
    XCTAssertNotNil(result.row, file: file, line: line)
    return result.row!
}

final class RegistryHlcTests: XCTestCase {
    func testOrdersByMsCounterDeviceLexicographically() {
        XCTAssertTrue(hlc(2) > hlc(1))
        XCTAssertTrue(encodeHlc(ms: 1, counter: 2, device: "a") > encodeHlc(ms: 1, counter: 1, device: "a"))
        XCTAssertTrue(encodeHlc(ms: 1, counter: 1, device: "b") > encodeHlc(ms: 1, counter: 1, device: "a"))
        // Fixed width: a 5-digit ms never compares below a 3-digit one.
        XCTAssertTrue(hlc(10000) > hlc(999))
        XCTAssertTrue(hlcNewer(hlc(1), nil))
        XCTAssertFalse(hlcNewer(hlc(1), hlc(1)))
    }
}

final class RegistryApplyOpTests: XCTestCase {
    func testCreatesRowsViaUpsertNeverViaUpdate() {
        let row = applied(nil, upsert())
        XCTAssertEqual(row.fields, ["title": .string("hello"), "archived": .bool(false)])
        XCTAssertEqual(row.clocks["title"], hlc(1000))
        XCTAssertFalse(row.deleted)

        var asUpdate = upsert()
        asUpdate.op = .update
        let miss = applyOp(nil, asUpdate)
        XCTAssertFalse(miss.changed)
        XCTAssertNil(miss.row)
    }

    func testFieldLevelLwwNewerWinsOlderLosesTiesLose() {
        var row = applied(nil, upsert())
        // Older write on one field: ignored, other fields untouched.
        let older = applyOp(row, update(set: ["title": .string("stale")], hlc: hlc(500)))
        XCTAssertFalse(older.changed)
        // Equal clock (same device, same instant): strict > means no-op — the
        // property that makes reconnect re-pushes idempotent.
        let replay = applyOp(row, upsert())
        XCTAssertFalse(replay.changed)
        // Newer write on one field leaves the other's clock alone.
        row = applied(row, update(set: ["title": .string("renamed")], hlc: hlc(2000, "dev-b")))
        XCTAssertEqual(row.fields, ["title": .string("renamed"), "archived": .bool(false)])
        XCTAssertEqual(row.clocks["archived"], hlc(1000))
    }

    func testSameMsConflictsSettleByDeviceIdDeterministically() {
        let base = applied(nil, upsert())
        let fromA = update(set: ["title": .string("A")], hlc: hlc(5000, "dev-a"))
        let fromB = update(set: ["title": .string("B")], hlc: hlc(5000, "dev-b"))
        let ab = applyOp(applyOp(base, fromA).row, fromB)
        let ba = applyOp(applyOp(base, fromB).row, fromA)
        // Same winner regardless of arrival order (dev-b > dev-a).
        XCTAssertEqual(ab.row?.fields["title"], .string("B"))
        XCTAssertEqual(ba.row?.fields["title"], .string("B"))
    }

    func testNullFieldValuesDeleteTheFieldStillClocked() {
        var row = applied(nil, upsert(set: ["title": .string("x"), "name": .string("y")]))
        row = applied(row, update(set: ["name": .null], hlc: hlc(2000)))
        XCTAssertEqual(row.fields, ["title": .string("x")])
        XCTAssertEqual(row.clocks["name"], hlc(2000))
        // A write older than the deletion cannot resurrect the field.
        let stale = applyOp(row, update(set: ["name": .string("zombie")], hlc: hlc(1500)))
        XCTAssertFalse(stale.changed)
    }

    func testDeleteTombstonesOnlyWhenCausallyNewerThanTheRow() {
        let row = applied(nil, upsert(hlc: hlc(1000)))
        // A delete older than the newest field write loses wholesale.
        let staleDelete = applyOp(row, deleteOp(hlc: hlc(500)))
        XCTAssertFalse(staleDelete.changed)
        // A newer delete wins and clears fields.
        let gone = applied(row, deleteOp(hlc: hlc(2000)))
        XCTAssertTrue(gone.deleted)
        XCTAssertEqual(gone.delHlc, hlc(2000))
        XCTAssertEqual(gone.fields, [:])
        // Updates never touch a tombstone.
        let touched = applyOp(gone, update(set: ["title": .string("ghost")], hlc: hlc(3000)))
        XCTAssertFalse(touched.changed)
        // An older upsert cannot revive it…
        let staleRevive = applyOp(gone, upsert(hlc: hlc(1500)))
        XCTAssertFalse(staleRevive.changed)
        // …a newer one can, and starts from ONLY its own fields.
        let revived = applied(gone, upsert(set: ["title": .string("back")], hlc: hlc(4000)))
        XCTAssertFalse(revived.deleted)
        XCTAssertEqual(revived.fields, ["title": .string("back")])
    }

    func testDeleteOnMissingRowPlantsGuardTombstone() {
        let gone = applied(nil, deleteOp(id: "chat-9", hlc: hlc(1000)))
        XCTAssertTrue(gone.deleted)
        // The guard blocks the late create it exists for.
        let late = applyOp(gone, upsert(id: "chat-9", hlc: hlc(500)))
        XCTAssertFalse(late.changed)
    }

    func testPerFieldClockOverridesPreserveOriginalCausality() {
        var row = applied(nil, upsert(set: ["title": .string("old"), "status": .string("idle")]))
        row = applied(row, update(set: ["status": .string("working")], hlc: hlc(9000)))
        // Re-seed the row elsewhere from its seed op…
        let seeded = applied(nil, rowToSeedOp(row))
        XCTAssertEqual(seeded.fields, row.fields)
        XCTAssertEqual(seeded.clocks, row.clocks)
        // …and a mid-history write STILL loses against the preserved clocks.
        let stale = applyOp(seeded, update(set: ["status": .string("errored")], hlc: hlc(5000)))
        XCTAssertFalse(stale.changed)
    }

    func testRowToSeedOpRoundTripsTombstones() {
        let gone = applied(nil, deleteOp(kind: "spaces", id: "sp-1", hlc: hlc(7000)))
        let seeded = applied(nil, rowToSeedOp(gone))
        XCTAssertTrue(seeded.deleted)
        XCTAssertEqual(seeded.delHlc, hlc(7000))
    }

    func testConvergesOverEveryCausallyValidArrivalOrder() {
        // The server sees each device's ops in push order, and a device only
        // updates rows it has seen — so the row's create always precedes the
        // updates. Permute everything AFTER the create (cross-device races).
        let create = upsert(hlc: hlc(1000))
        let races: [RegistryOp] = [
            update(set: ["title": .string("renamed")], hlc: hlc(3000, "dev-b")),
            update(set: ["archived": .bool(true)], hlc: hlc(2000, "dev-c")),
            upsert(set: ["title": .string("other"), "cwd": .string("/tmp")], hlc: hlc(2500, "dev-d")),
        ]
        var outcomes: [RegistryRow] = []
        func permute(_ rest: [RegistryOp], _ acc: [RegistryOp]) {
            if rest.isEmpty {
                var row: RegistryRow?
                for op in [create] + acc {
                    row = applyOp(row, op).row ?? row
                }
                outcomes.append(row!)
                return
            }
            for ix in rest.indices {
                var next = rest
                next.remove(at: ix)
                permute(next, acc + [rest[ix]])
            }
        }
        permute(races, [])
        XCTAssertEqual(outcomes.count, 6)
        for outcome in outcomes {
            XCTAssertEqual(outcome.fields, outcomes[0].fields)
            XCTAssertEqual(outcome.clocks, outcomes[0].clocks)
            XCTAssertFalse(outcome.deleted)
        }
        XCTAssertEqual(outcomes[0].fields["title"], .string("renamed"))
        XCTAssertEqual(outcomes[0].fields["archived"], .bool(true))
        XCTAssertEqual(outcomes[0].fields["cwd"], .string("/tmp"))
    }

    func testDropsAnUpdateThatOutrunsItsRowsCreate() {
        // Causally impossible on the wire (per-device batches are ordered),
        // but a hand-crafted client could send it: dropped, not deferred.
        let early = applyOp(nil, update(set: ["title": .string("too early")], hlc: hlc(3000)))
        XCTAssertFalse(early.changed)
        let row = applied(nil, upsert(hlc: hlc(1000)))
        XCTAssertEqual(row.fields["title"], .string("hello"))
    }
}

final class RegistryValidateOpTests: XCTestCase {
    func testAcceptsWellFormedOpsAndRejectsMalformedOnes() {
        XCTAssertNil(validateOp(upsert()))

        var badKind = upsert()
        badKind.kind = "Nope Kind"
        XCTAssertTrue(validateOp(badKind)?.contains("kind") == true)

        var badId = upsert()
        badId.id = ""
        XCTAssertTrue(validateOp(badId)?.contains("id") == true)

        // (The "bad op" vector guards JS's untyped `op` field; Swift's enum
        // makes that state unrepresentable.)

        var badHlc = upsert()
        badHlc.hlc = "not-a-clock"
        XCTAssertTrue(validateOp(badHlc)?.contains("hlc") == true)

        var noSet = upsert()
        noSet.set = nil
        XCTAssertTrue(validateOp(noSet)?.contains("set") == true)

        var badField = upsert()
        badField.set = ["bad field!": .int(1)]
        XCTAssertTrue(validateOp(badField)?.contains("field") == true)

        var deleteWithSet = deleteOp(id: "c", hlc: hlc(1))
        deleteWithSet.set = [:]
        XCTAssertTrue(validateOp(deleteWithSet)?.contains("delete") == true)

        var badClock = upsert()
        badClock.clocks = ["title": "junk"]
        XCTAssertTrue(validateOp(badClock)?.contains("clock") == true)

        let huge = upsert(set: ["blob": .string(String(repeating: "x", count: 20_000))])
        XCTAssertTrue(validateOp(huge)?.contains("large") == true)
    }
}

final class RegistryMaxClockTests: XCTestCase {
    func testReturnsTheNewestClockAcrossFieldsAndTombstone() {
        let row = applied(nil, upsert(set: ["a": .int(1), "b": .int(2)], hlc: hlc(1000)))
        let bumped = applied(row, update(set: ["b": .int(3)], hlc: hlc(5000)))
        XCTAssertEqual(maxClock(bumped), hlc(5000))
        let tomb = RegistryRow(kind: "x", id: "y", seq: 0, deleted: true,
                               delHlc: hlc(9), fields: [:], clocks: [:])
        XCTAssertEqual(maxClock(tomb), hlc(9))
    }
}

final class RegistryLifecycleProofTests: XCTestCase {
    func testProofRidesTheTombstoneAndClearsOnRevival() {
        let proof = JSONValue.object(["e1": .string("c2VhbGVk")])
        var del = deleteOp(hlc: hlc(2000))
        del.proof = proof
        let gone = applied(applied(nil, upsert()), del)
        XCTAssertTrue(gone.deleted)
        XCTAssertEqual(gone.delProof, proof)
        var newer = deleteOp(hlc: hlc(3000))
        newer.proof = .object(["e1": .string("bmV3ZXI=")])
        let again = applied(gone, newer)
        XCTAssertEqual(again.delProof, newer.proof)
        XCTAssertFalse(applyOp(again, del).changed)
        XCTAssertEqual(rowToSeedOp(again).proof, newer.proof)
        let revived = applied(again, upsert(hlc: hlc(4000)))
        XCTAssertFalse(revived.deleted)
        XCTAssertNil(revived.delProof)
        XCTAssertEqual(revived.delHlc, hlc(3000))
    }

    func testValidateOpRefusesProofsOutsideDeletes() {
        var op = upsert()
        op.proof = .object(["e1": .string("x")])
        XCTAssertEqual(validateOp(op), "bad proof")
        var del = deleteOp(hlc: hlc(1))
        del.proof = .string("not an envelope")
        XCTAssertEqual(validateOp(del), "bad proof")
        del.proof = .object(["e1": .string("x")])
        XCTAssertNil(validateOp(del))
    }
}
