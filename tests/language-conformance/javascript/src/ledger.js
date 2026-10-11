import { scale } from "./helper.js";

function clamp(value) {
  return Math.max(value, 0);
}

/** Sum the values, scaled and clamped. */
export function total(values) {
  function double(x) {
    return x * 2;
  }
  let result = 0;
  for (const value of values) {
    result += clamp(scale(double(value), 2));
  }
  return result;
}

export const average = (values) => total(values) / values.length;

export class Account {
  balance = 0;

  deposit(amount) {
    if (this.validate(amount)) {
      this.balance += amount;
    }
  }

  validate(amount) {
    return amount > 0;
  }
}
