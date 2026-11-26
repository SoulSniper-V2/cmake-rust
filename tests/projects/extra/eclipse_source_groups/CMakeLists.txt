cmake_minimum_required(VERSION 3.20)
project(groups C)
# Groups are kept in creation order (not sorted), regexes are matched with
# the most recently created group first, and subdirectories start with a
# copy of their parent's groups.
source_group("Zeta Files" FILES main.c)
source_group("Alpha Files" FILES util.c)
source_group("Zregex" REGULAR_EXPRESSION "\\.h$")
source_group("Aregex" REGULAR_EXPRESSION "x\\.h$")
source_group("Outer\\Inner" FILES extra.c)
add_executable(app main.c util.c extra.c a.h x.h)
add_subdirectory(sub)
source_group("Late" FILES sub/s.c)
