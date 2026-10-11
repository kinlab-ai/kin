#include <algorithm>
#include <vector>

#include "helper.hpp"

namespace shop {

static int clamp(int value) {
    return std::max(value, 0);
}

/** Sum the values, scaled and clamped. */
[[nodiscard]] int total(const std::vector<int>& values) {
    auto twice = [](int x) { return x * 2; };
    int result = 0;
    for (int value : values) {
        result += clamp(scale(twice(value), 2));
    }
    return result;
}

class Account {
public:
    void deposit(int amount) {
        if (this->validate(amount)) {
            balance_ += amount;
        }
    }

    bool validate(int amount) const {
        return amount > 0;
    }

private:
    int balance_ = 0;
};

}  // namespace shop
