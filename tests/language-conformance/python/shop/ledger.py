import functools

from shop.helper import scale


@functools.cache
def clamp(value):
    return max(value, 0)


def total(values):
    """Sum the values, scaled and clamped."""

    def double(x):
        return x * 2

    result = 0
    for value in values:
        result += clamp(scale(double(value), 2))
    return result


class Account:
    """A running balance."""

    def __init__(self):
        self.balance = 0

    def deposit(self, amount):
        if self.validate(amount):
            self.balance += amount

    def validate(self, amount):
        return amount > 0
