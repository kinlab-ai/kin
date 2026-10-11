let square: (Int) -> Int = { value in value * value }

func fetch(_ url: String) async -> String {
    url
}

func parse(_ text: String) -> Int {
    Int(text) ?? 0
}

func parse(_ value: Double) -> Int {
    Int(value)
}

struct Point {
    var x = 0
    let y = 0

    var norm: Int {
        abs(x) + abs(y)
    }

    @inlinable
    func length() -> Int {
        norm
    }

    struct Inner {
        func ping() -> String {
            "pong"
        }
    }
}
