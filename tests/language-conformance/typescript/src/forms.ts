export function* countUp(limit: number): Generator<number> {
  for (let value = 0; value < limit; value++) {
    yield value;
  }
}

export async function fetchText(url: string): Promise<string> {
  return url;
}

export const square = (value: number): number => value * value;

export default function main(): number {
  return square(2);
}

export function parse(text: string): number;
export function parse(text: number): number;
export function parse(text: string | number): number {
  return Number(text);
}

export namespace Outer {
  export class Inner {
    ping(): string {
      return "pong";
    }
  }
}

function sealed(constructor: Function): void {
  Object.seal(constructor);
}

@sealed
export class Point {
  x = 0;
  private readonly y: number = 0;

  get norm(): number {
    return Math.abs(this.x) + Math.abs(this.y);
  }
}

export const res: Record<string, unknown> = {};

res.sendFile = function sendFile(path: string, options?: Record<string, unknown>) {
  const opts: Record<string, unknown> = options || {};
  opts.root = path;
  return opts;
};
