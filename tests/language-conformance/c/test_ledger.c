#include <assert.h>

int total(const int *values, int count);

void test_total(void) {
    const int values[] = {1, 2};
    assert(total(values, 2) == 12);
}

int main(void) {
    test_total();
    return 0;
}
