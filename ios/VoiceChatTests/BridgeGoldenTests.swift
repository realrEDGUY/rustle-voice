import Foundation
import XCTest
@testable import VoiceChat

/// Checks the Swift mirrors against the golden files the Rust tests write
/// (crates/vc-mobile/tests/golden): the snapshot and live views decode, and
/// re-encode to the same JSON (so no field is missing or misnamed), and every
/// command encodes exactly as listed in commands.json.
final class BridgeGoldenTests: XCTestCase {
    private func golden(_ name: String) throws -> Data {
        // .../ios/VoiceChatTests/BridgeGoldenTests.swift -> repo root
        let root = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let url = root.appendingPathComponent("crates/vc-mobile/tests/golden").appendingPathComponent(name)
        return try Data(contentsOf: url)
    }

    /// JSON as Foundation objects with `null` members dropped (Swift omits nil).
    private func normalized(_ data: Data) throws -> NSObject {
        func strip(_ v: Any) -> Any {
            if let d = v as? [String: Any] {
                var out: [String: Any] = [:]
                for (k, x) in d where !(x is NSNull) { out[k] = strip(x) }
                return out as NSDictionary
            }
            if let a = v as? [Any] { return a.map(strip) as NSArray }
            return v
        }
        let obj = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        return strip(obj) as! NSObject
    }

    func testSnapshotDecodesAndRoundTrips() throws {
        let data = try golden("snapshot.json")
        let snap = try VCJSON.decoder.decode(Snapshot.self, from: data)
        XCTAssertEqual(snap.selected, 1)
        XCTAssertEqual(snap.settings.inputMode, "Ptt")
        XCTAssertEqual(snap.servers.count, 2)
        let relay = try XCTUnwrap(snap.server(1))
        XCTAssertTrue(relay.status.isConnected)
        XCTAssertEqual(relay.channels.map(\.name), ["Lobby", "Gaming"])
        XCTAssertFalse(relay.channels[1].permanent)
        let bob = try XCTUnwrap(relay.user(2))
        XCTAssertTrue(bob.sharing && bob.watching && bob.streamMuted)
        XCTAssertEqual(bob.volume, 1.5)
        XCTAssertEqual(relay.chat.count, 3)
        XCTAssertEqual(relay.chat[1].image?.available, true)
        XCTAssertEqual(relay.chat[2].image?.available, false)
        XCTAssertEqual(snap.tofu?.label, "Locked")
        XCTAssertEqual(snap.streams.first?.id, "1/2")
        XCTAssertEqual(snap.server(2)?.status.kind, "failed")

        let again = try VCJSON.encoder.encode(snap)
        XCTAssertEqual(try normalized(again), try normalized(data), "Snapshot.swift is missing or misnaming a field")
    }

    func testLiveDecodesAndRoundTrips() throws {
        let data = try golden("live.json")
        let live = try VCJSON.decoder.decode(Live.self, from: data)
        XCTAssertTrue(live.transmitting)
        XCTAssertEqual(live.inputLevelDb, -23)
        XCTAssertEqual(live.server(1)?.pingMs, 42)
        XCTAssertTrue(live.isSpeaking(1, 2))
        XCTAssertFalse(live.isSpeaking(2, 2))
        let again = try VCJSON.encoder.encode(live)
        XCTAssertEqual(try normalized(again), try normalized(data), "Live.swift is missing or misnaming a field")
    }

    /// The same commands, in the same order, as commands.json.
    static let commands: [Command] = [
        .select(key: 1),
        .connect(key: 1),
        .disconnect(key: 1),
        .talkHere(key: 2),
        .removeServer(key: 2),
        .saveServer(editing: nil, label: "Relay", addr: "127.0.0.1:7878", nick: "ios", password: "", connectNow: true),
        .join(key: 1, channel: 5),
        .createChannel(key: 1, name: "Gaming"),
        .renameChannel(key: 1, channel: 5, name: "Games"),
        .deleteChannel(key: 1, channel: 5),
        .sendChat(key: 1, text: "hello"),
        .toggleMute,
        .toggleDeafen,
        .setInputMode(mode: "Ptt"),
        .setVadThreshold(db: -40.5, persist: false),
        .setMasterVolume(volume: 1.25, persist: true),
        .setNickname(nick: "phone"),
        .setUserVolume(key: 1, nick: "bob", volume: 1.5),
        .muteUser(key: 1, nick: "bob", muted: true),
        .watchScreen(key: 1, user: 2, on: true),
        .muteStream(key: 1, user: 2, muted: false),
        .uploadFile(key: 1, path: "/tmp/vc-uploads/a.jpg"),
        .trustNewIdentity,
        .cancelTofu,
        .dismissToast(text: "Error: Not connected"),
    ]

    func testCommandsEncodeLikeGolden() throws {
        let data = try golden("commands.json")
        let want = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [Any])
        XCTAssertEqual(want.count, Self.commands.count, "commands.json and BridgeGoldenTests.commands differ in length")
        for (i, (c, w)) in zip(Self.commands, want).enumerated() {
            let got = try JSONSerialization.jsonObject(with: Data(c.json().utf8)) as! NSDictionary
            XCTAssertEqual(got, w as? NSDictionary, "command #\(i): \(c.json())")
        }
    }

    func testTheme() {
        XCTAssertEqual(Theme.initials("Relay server"), "RS")
        XCTAssertEqual(Theme.initials("bob"), "BO")
        XCTAssertEqual(Theme.utcTime(1_700_000_000_000), "22:13:20 UTC")
        XCTAssertEqual(StreamsView.gridDims(5).cols, 3)
        XCTAssertEqual(StreamsView.gridDims(4).cols, 2)
    }

    @MainActor
    func testVersionFromCore() {
        XCTAssertFalse(String(cString: vc_version()).isEmpty)
    }
}
