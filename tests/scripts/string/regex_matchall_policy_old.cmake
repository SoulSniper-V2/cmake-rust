# CMP0186 OLD: `^` may match at the offset of repeated searches.
cmake_policy(SET CMP0186 OLD)
string(REGEX MATCHALL "(^|,)" a "a,b")
message("a=<${a}>")
string(REGEX REPLACE "(^|,)" "<" c "a,b")
message("c=<${c}>")
