import { total } from "./ledger.js";

test("total", () => {
  expect(total([1, 2])).toBe(12);
});
