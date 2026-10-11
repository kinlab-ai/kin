export function* countUp(limit) {
  for (let value = 0; value < limit; value++) {
    yield value;
  }
}

export async function fetchText(url) {
  return url;
}

export const square = (value) => value * value;

export default function main() {
  return square(2);
}

function sealed(value) {
  return value;
}

@sealed
class Point {
  x = 0;
  #y = 0;

  get norm() {
    return Math.abs(this.x) + Math.abs(this.#y);
  }
}

export { Point };

export const res = {};

res.sendFile = function sendFile(path, options) {
  const opts = options || {};
  opts.root = path;
  return opts;
};
