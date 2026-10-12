import { total } from "./ledger";

test("total", () => {
  expect(total([1, 2])).toBe(12);
});
