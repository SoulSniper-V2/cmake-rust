cmake_minimum_required(VERSION 3.25)
# ASM and ASM-ATT both list ".s"; the language enabled last (ASM-ATT) claims
# it, so this target mixes ASM and ASM-ATT at the same linker preference.
project(asm LANGUAGES C ASM ASM-ATT)
add_library(a STATIC a.s b.S)
add_executable(e main.c)
target_link_libraries(e PRIVATE a)
