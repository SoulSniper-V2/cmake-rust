cmake_minimum_required(VERSION 4.1)
execute_process(
  COMMAND python3 -c [=[
import os

os.makedirs(b"tree\x80")
for name in (b"a.txt", b"\x80.txt", b"\xc3\xa9.txt", b"tree\x80/nested.txt"):
    with open(name, "wb"):
        pass
]=]
  COMMAND_ERROR_IS_FATAL ANY)

file(GLOB_RECURSE paths RELATIVE "${CMAKE_CURRENT_SOURCE_DIR}" "*.txt")
string(HEX "${paths}" paths_hex)
message("${paths_hex}")

string(ASCII 128 name)
file(GLOB exact RELATIVE "${CMAKE_CURRENT_SOURCE_DIR}" "${name}.txt")
string(HEX "${exact}" exact_hex)
message("${exact_hex}")
