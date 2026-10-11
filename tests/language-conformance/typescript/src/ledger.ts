import { scale } from "./helper";

function clamp(value: number): number {
  return Math.max(value, 0);
}

/** Sum the values, scaled and clamped. */
export function total(values: number[]): number {
  function double(x: number): number {
    return x * 2;
  }
  let result = 0;
  for (const value of values) {
    result += clamp(scale(double(value), 2));
  }
  return result;
}

export const average = (values: number[]): number => total(values) / values.length;

export class Account {
  balance = 0;

  deposit(amount: number): void {
    if (this.validate(amount)) {
      this.balance += amount;
    }
  }

  validate(amount: number): boolean {
    return amount > 0;
  }
}
