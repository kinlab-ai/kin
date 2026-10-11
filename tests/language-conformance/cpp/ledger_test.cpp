#include <gtest/gtest.h>

#include <vector>

namespace shop {
int total(const std::vector<int>& values);
}

TEST(LedgerTest, Total) {
    EXPECT_EQ(shop::total({1, 2}), 12);
}
