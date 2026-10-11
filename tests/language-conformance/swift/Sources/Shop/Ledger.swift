import ShopUtil

func clamp(_ value: Int) -> Int {
    max(value, 0)
}

/// Sum the values, scaled and clamped.
@discardableResult
public func total(_ values: [Int]) -> Int {
    func double(_ x: Int) -> Int {
        x * 2
    }
    var result = 0
    for value in values {
        result += clamp(scale(double(value), 2))
    }
    return result
}

public struct Account {
    var balance = 0

    public mutating func deposit(_ amount: Int) {
        if self.validate(amount) {
            balance += amount
        }
    }

    func validate(_ amount: Int) -> Bool {
        amount > 0
    }
}
