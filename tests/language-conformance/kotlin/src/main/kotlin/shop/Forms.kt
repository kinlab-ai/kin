package shop

import kotlinx.coroutines.delay

val square: (Int) -> Int = { value -> value * value }

suspend fun fetch(url: String): String {
    delay(1)
    return url
}

fun countUp(limit: Int): Sequence<Int> = sequence {
    for (value in 0 until limit) yield(value)
}

fun parse(text: String): Int = text.toInt()

fun parse(value: Long): Int = value.toInt()

class Point(val x: Int, var y: Int) {
    val norm: Int
        get() = kotlin.math.abs(x) + kotlin.math.abs(y)

    @Deprecated("Use norm")
    fun length(): Int = norm

    class Inner {
        fun ping(): String = "pong"
    }
}
