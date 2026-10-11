package shop

import shop.util.scale

fun clamp(value: Int): Int = maxOf(value, 0)

/** Sum the values, scaled and clamped. */
@Suppress("unused")
fun total(values: List<Int>): Int {
    fun double(x: Int): Int = x * 2
    var result = 0
    for (value in values) {
        result += clamp(scale(double(value), 2))
    }
    return result
}

class Account {
    var balance = 0

    fun deposit(amount: Int) {
        if (this.validate(amount)) {
            balance += amount
        }
    }

    fun validate(amount: Int): Boolean = amount > 0
}
