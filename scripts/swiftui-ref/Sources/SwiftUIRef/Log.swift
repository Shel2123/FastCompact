import Foundation

/// Writes to stdout and to `~/Library/Logs/Orbit/swiftui-ref.log`.
enum Log {
    static let url = FileManager.default.homeDirectoryForCurrentUser
        .appending(path: "Library/Logs/Orbit/swiftui-ref.log")

    static func write(_ line: String) {
        print(line)
        fflush(stdout)
        let data = Data((line + "\n").utf8)
        do {
            try FileManager.default.createDirectory(
                at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            if let handle = try? FileHandle(forWritingTo: url) {
                defer { try? handle.close() }
                try handle.seekToEnd()
                try handle.write(contentsOf: data)
            } else {
                try data.write(to: url)
            }
        } catch {
            FileHandle.standardError.write(Data("log write failed: \(error)\n".utf8))
        }
    }

    static func fail(_ line: String) {
        FileHandle.standardError.write(Data((line + "\n").utf8))
        write(line)
    }
}
