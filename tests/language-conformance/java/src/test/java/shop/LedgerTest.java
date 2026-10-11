package shop;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class LedgerTest {
    @Test
    void testTotal() {
        assertEquals(12, Ledger.total(new int[] {1, 2}));
    }
}
