// The mention wire format, ported from crates/ui/src/composer.rs:670-795.
// A private URI scheme keeps file mentions distinguishable from ordinary
// Markdown links pasted into the composer.
//
// This must agree with the Rust byte for byte. The desktop parser rejects a
// link whose label is not the path's basename, whose path is unsafe, or whose
// encoding does not round-trip — and it parses the WHOLE draft, so a stray
// bracket elsewhere can invalidate an otherwise perfect link. ComposerText
// relies on `parse` to notice exactly that.
//
// Offsets count Characters, matching ComposerTrigger.

import Foundation

struct ParsedMention: Equatable {
    let range: Range<Int>
    let basename: String
    let path: String
    let isDir: Bool
}

enum MentionLink {

    static let scheme = "zeron-file:"

    // MARK: Encoding

    static func percentEncode(_ path: String) -> String {
        var out = ""
        for byte in Array(path.utf8) {
            let unreserved = (byte >= 0x30 && byte <= 0x39)   // 0-9
                || (byte >= 0x41 && byte <= 0x5A)             // A-Z
                || (byte >= 0x61 && byte <= 0x7A)             // a-z
                || byte == 0x2D || byte == 0x2E               // - .
                || byte == 0x5F || byte == 0x7E               // _ ~
                || byte == 0x2F                               // /
            if unreserved {
                out.append(Character(UnicodeScalar(byte)))
            } else {
                out.append("%")
                out += String(format: "%02X", byte)
            }
        }
        return out
    }

    static func percentDecode(_ encoded: String) -> String? {
        var bytes: [UInt8] = []
        let raw = Array(encoded.utf8)
        var at = 0
        while at < raw.count {
            if raw[at] == UInt8(ascii: "%") {
                guard at + 2 < raw.count,
                      let hex = String(bytes: raw[(at + 1)...(at + 2)], encoding: .utf8),
                      hex.allSatisfy(\.isHexDigit),
                      let byte = UInt8(hex, radix: 16)
                else { return nil }
                bytes.append(byte)
                at += 3
            } else {
                bytes.append(raw[at])
                at += 1
            }
        }
        return String(bytes: bytes, encoding: .utf8)
    }

    // MARK: Labels and safety

    static func escapeLabel(_ label: String) -> String {
        label
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "[", with: "\\[")
            .replacingOccurrences(of: "]", with: "\\]")
    }

    static func isSafe(_ path: String) -> Bool {
        guard !path.isEmpty,
              !path.hasPrefix("/"),
              !path.contains("\\"),
              !path.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })
        else { return false }
        for part in path.split(separator: "/", omittingEmptySubsequences: false)
        where part.isEmpty || part == "." || part == ".." {
            return false
        }
        return true
    }

    static func basename(of path: String) -> String {
        let trimmed = trimTrailingSlashes(path)
        guard let last = trimmed.split(separator: "/").last else { return trimmed }
        return String(last)
    }

    // MARK: Serialize

    static func serialize(path rawPath: String, isDir: Bool) -> String {
        let path = trimTrailingSlashes(rawPath)
        let target = path + (isDir ? "/" : "")
        return "[\(escapeLabel(basename(of: path)))](\(scheme)\(percentEncode(target)))"
    }

    // MARK: Parse

    static func parse(_ text: String) -> [ParsedMention] {
        let chars = Array(text)
        var links: [ParsedMention] = []
        var search = 0

        while search < chars.count {
            guard let start = chars[search...].firstIndex(of: "[") else { break }
            guard let labelEnd = labelClose(chars, from: start + 1) else {
                search = start + 1
                continue
            }
            let targetStart = labelEnd + 2
            guard targetStart <= chars.count,
                  let closeIndex = chars[targetStart...].firstIndex(of: ")") else {
                search = start + 1
                continue
            }
            let end = closeIndex + 1
            let label = String(chars[(start + 1)..<labelEnd])
            let target = String(chars[targetStart..<(end - 1)])

            guard target.hasPrefix(scheme) else {
                search = end
                continue
            }
            let encoded = String(target.dropFirst(scheme.count))

            if let decoded = percentDecode(encoded) {
                let isDir = decoded.hasSuffix("/")
                let path = isDir ? String(decoded.dropLast()) : decoded
                let name = basename(of: path)
                if isSafe(path),
                   percentEncode(decoded) == encoded,
                   escapeLabel(name) == label {
                    links.append(ParsedMention(range: start..<end, basename: name,
                                               path: path, isDir: isDir))
                }
            }
            search = end
        }
        return links
    }

    // MARK: Helpers

    /// The first unescaped `]` that is followed by `(`.
    private static func labelClose(_ chars: [Character], from start: Int) -> Int? {
        var escaped = false
        var at = start
        while at < chars.count {
            if escaped {
                escaped = false
            } else if chars[at] == "\\" {
                escaped = true
            } else if chars[at] == "]", at + 1 < chars.count, chars[at + 1] == "(" {
                return at
            }
            at += 1
        }
        return nil
    }

    private static func trimTrailingSlashes(_ path: String) -> String {
        var out = path
        while out.hasSuffix("/") { out.removeLast() }
        return out
    }
}
