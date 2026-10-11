import XCTest
@testable import Shop

final class LedgerTests: XCTestCase {
    func testTotal() {
        XCTAssertEqual(total([1, 2]), 12)
    }
}
