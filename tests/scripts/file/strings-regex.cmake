cmake_policy(SET CMP0159 NEW)
file(STRINGS data/input out REGEX "(p)([a-z]+)")
message("[${out}] ${CMAKE_MATCH_COUNT}:${CMAKE_MATCH_1}:${CMAKE_MATCH_2}")
