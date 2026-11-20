cmake_policy(SET CMP0159 NEW)
file(STRINGS data/input out LIMIT_INPUT -1)
message("[${out}] ${CMAKE_MATCH_COUNT}:${CMAKE_MATCH_1}:${CMAKE_MATCH_2}")
