#include <string>

namespace shop {

auto square = [](int value) { return value * value; };

int parse(const std::string& text) {
    return std::stoi(text);
}

int parse(long value) {
    return static_cast<int>(value);
}

class Point {
public:
    [[nodiscard]] int norm() const {
        return x + y;
    }

    class Inner {
    public:
        std::string ping() const {
            return "pong";
        }
    };

    int x = 0;
    int y = 0;
};

}  // namespace shop
