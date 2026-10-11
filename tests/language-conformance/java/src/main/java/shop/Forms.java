package shop;

import java.util.function.IntUnaryOperator;

public class Forms {
    public static final IntUnaryOperator SQUARE = value -> value * value;

    private int count;
    protected final String label = "forms";

    public static int parse(String text) {
        return Integer.parseInt(text);
    }

    public static int parse(long value) {
        return (int) value;
    }

    @Deprecated
    public int getCount() {
        return count;
    }

    public static class Inner {
        public String ping() {
            return "pong";
        }
    }
}
