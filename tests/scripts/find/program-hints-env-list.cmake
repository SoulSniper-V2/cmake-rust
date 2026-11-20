cmake_minimum_required(VERSION 4.1)
set(ENV{FIND_TEST_DIRS} "data/missing::data/dir")
find_program(result NAMES needle.tool HINTS ENV FIND_TEST_DIRS NO_DEFAULT_PATH)
message("picked=[${result}], cache=[$CACHE{result}]")
