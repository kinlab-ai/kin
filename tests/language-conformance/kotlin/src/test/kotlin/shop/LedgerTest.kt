package shop

import kotlin.test.Test
import kotlin.test.assertEquals

class LedgerTest {
    @Test
    fun testTotal() {
        assertEquals(12, total(listOf(1, 2)))
    }
}
