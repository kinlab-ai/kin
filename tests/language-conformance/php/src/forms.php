<?php

namespace Shop;

function count_up(int $limit): \Generator
{
    for ($value = 0; $value < $limit; $value++) {
        yield $value;
    }
}

#[\Attribute]
final class Marker
{
}

class Point
{
    public int $x = 0;
    private int $y = 0;

    public function __construct(public readonly string $label = 'point')
    {
    }

    #[Marker]
    public function norm(): int
    {
        return abs($this->x) + abs($this->y);
    }
}
