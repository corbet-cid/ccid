// The C++ probe: libc++'s std::string, so the link really needs libc++.so.1 and libc++abi.so.1.
#include <cstdio>
#include <string>

int main() {
    std::string text("check-shell probe: c++");
    text += " ok";
    std::puts(text.c_str());
    return 0;
}
