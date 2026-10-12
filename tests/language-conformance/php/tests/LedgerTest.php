<?php

namespace Shop\Tests;

use PHPUnit\Framework\TestCase;

use function Shop\total;

final class LedgerTest extends TestCase
{
    public function testTotal(): void
    {
        $this->assertSame(12, total([1, 2]));
    }
}
