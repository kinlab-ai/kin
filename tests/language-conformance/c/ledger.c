#include "helper.h"

static int clamp(int value) {
    return value < 0 ? 0 : value;
}

static int twice(int x) {
    return x * 2;
}

/** Sum the values, scaled and clamped. */
int total(const int *values, int count) {
    int result = 0;
    for (int i = 0; i < count; i++) {
        result += clamp(scale(twice(values[i]), 2));
    }
    return result;
}

struct account {
    int balance;
};

static int account_validate(const struct account *a, int amount) {
    (void)a;
    return amount > 0;
}

void account_deposit(struct account *a, int amount) {
    if (account_validate(a, amount)) {
        a->balance += amount;
    }
}
