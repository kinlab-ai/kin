// @flow
import type { Node } from "react";

export type Totals = {| count: number, sum: number |};

export opaque type Cents: number = number;

export function total(values: Array<number>, label: ?string): number {
  let sum = 0;
  for (const value of values) {
    sum += value;
  }
  return sum;
}

export hook useCount(initial: number): number {
  return initial;
}

export component Badge(label: string, count: number = 0) renders Node {
  return label;
}
