package shop;

import shop.util.Helper;

public final class Ledger {
    private Ledger() {}

    static int clamp(int value) {
        return Math.max(value, 0);
    }

    /** Sum the values, scaled and clamped. */
    @SuppressWarnings("unused")
    public static int total(int[] values) {
        class Doubler {
            int apply(int x) {
                return x * 2;
            }
        }
        Doubler doubler = new Doubler();
        int result = 0;
        for (int value : values) {
            result += clamp(Helper.scale(doubler.apply(value), 2));
        }
        return result;
    }
}

final class Account {
    private int balance;

    void deposit(int amount) {
        if (this.validate(amount)) {
            this.balance += amount;
        }
    }

    boolean validate(int amount) {
        return amount > 0;
    }
}
