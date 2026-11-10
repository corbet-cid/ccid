// The probe's C++ dependency, compiled against libc++ by build.rs.
#include <string>

extern "C" int probe_cxx_len(const char *text) {
    std::string value(text);
    value += "!";
    return static_cast<int>(value.size());
}
