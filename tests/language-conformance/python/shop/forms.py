import dataclasses
import typing


def count_up(limit):
    yield from range(limit)


async def fetch(url):
    return url


square = lambda value: value * value


@typing.overload
def parse(text: str) -> int: ...


@typing.overload
def parse(text: bytes) -> int: ...


def parse(text):
    return int(text)


class Outer:
    class Inner:
        def ping(self):
            return "pong"


@dataclasses.dataclass
class Point:
    x: int = 0
    y: int = 0

    @property
    def norm(self):
        return abs(self.x) + abs(self.y)
