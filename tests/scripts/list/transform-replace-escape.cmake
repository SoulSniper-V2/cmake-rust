cmake_minimum_required(VERSION 4.1)
set(items a)
list(TRANSFORM items REPLACE "a" [[\é]])
