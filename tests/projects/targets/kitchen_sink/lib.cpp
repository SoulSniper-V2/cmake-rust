#include "lib_export.h"
#include <vector>
LIB_EXPORT int f(){ std::vector<int> v{1}; return v[0]; }
