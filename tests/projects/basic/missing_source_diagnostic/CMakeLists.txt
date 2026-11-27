cmake_minimum_required(VERSION 4.1)
project(MissingSource C)
add_library(missing STATIC does-not-exist.c)
