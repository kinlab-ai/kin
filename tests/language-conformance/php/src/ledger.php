<?php

namespace Shop;

use function Shop\Util\scale;

function clamp(int $value): int
{
    return max($value, 0);
}

/** Sum the values, scaled and clamped. */
#[\Deprecated]
function total(array $values): int
{
    $double = fn (int $x): int => $x * 2;
    $result = 0;
    foreach ($values as $value) {
        $result += clamp(scale($double($value), 2));
    }
    return $result;
}

class Account
{
    private int $balance = 0;

    public function deposit(int $amount): void
    {
        if ($this->validate($amount)) {
            $this->balance += $amount;
        }
    }

    public function validate(int $amount): bool
    {
        return $amount > 0;
    }
}
