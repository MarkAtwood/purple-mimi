# SPDX-License-Identifier: GPL-3.0-or-later
#
# One build, two plugins from one static Rust library
# (docs/design/purple-mimi.md §10.1; decision PQ8): cargo is called
# directly, with no Corrosion and no network at configure time.
#
#   cmake -B build && cmake --build build
#
# Options:
#   PURPLE2        the libpurple 2.14 plugin   (default: ON if found)
#   PURPLE3        the libpurple 3 plugin      (default: ON if found)
#   CARGO_OFFLINE  pass --offline to cargo, for distro builds with
#                  vendored crates
#   CARGO_PROFILE  release (default) or dev

cmake_minimum_required(VERSION 3.20)
project(purple-mimi VERSION 0.1.0 LANGUAGES C)

find_package(PkgConfig REQUIRED)
find_program(CARGO cargo REQUIRED)

option(CARGO_OFFLINE "Pass --offline to cargo" OFF)
set(CARGO_PROFILE "release" CACHE STRING "cargo profile: release or dev")

# libpurple 2 is "purple"; libpurple 3 is "purple-3".
pkg_check_modules(PURPLE2_PC QUIET IMPORTED_TARGET purple)
pkg_check_modules(PURPLE3_PC QUIET IMPORTED_TARGET purple-3)

option(PURPLE2 "Build the libpurple 2.14 plugin" ${PURPLE2_PC_FOUND})
option(PURPLE3 "Build the libpurple 3 plugin" ${PURPLE3_PC_FOUND})

if(PURPLE2 AND NOT PURPLE2_PC_FOUND)
  message(FATAL_ERROR "PURPLE2=ON but pkg-config cannot find 'purple'")
endif()
if(PURPLE3 AND NOT PURPLE3_PC_FOUND)
  message(FATAL_ERROR "PURPLE3=ON but pkg-config cannot find 'purple-3'")
endif()
if(NOT PURPLE2 AND NOT PURPLE3)
  message(FATAL_ERROR
          "Neither libpurple 2 nor libpurple 3 was found; install "
          "libpurple-dev (Debian) or pidgin (Homebrew)")
endif()

# --- the Rust static library -------------------------------------------
#
# A staticlib, so each plugin needs only libpurple, GLib and libc
# (§10.1). catch_unwind at the ABI needs unwinding, which the workspace's
# profiles guarantee; never build this with panic = "abort".

if(CARGO_PROFILE STREQUAL "release")
  set(cargo_flags --release)
  set(cargo_dir release)
else()
  set(cargo_flags "")
  set(cargo_dir debug)
endif()
if(CARGO_OFFLINE)
  list(APPEND cargo_flags --offline)
endif()

set(cargo_target "${CMAKE_CURRENT_BINARY_DIR}/cargo")
set(ffi_lib "${cargo_target}/${cargo_dir}/libpurple_mimi_ffi.a")

# Every Rust source, so cargo re-runs when one changes.
file(GLOB_RECURSE rust_sources
     "${CMAKE_CURRENT_SOURCE_DIR}/core/src/*.rs"
     "${CMAKE_CURRENT_SOURCE_DIR}/ffi/src/*.rs")

add_custom_command(
  OUTPUT "${ffi_lib}"
  COMMAND ${CMAKE_COMMAND} -E env "CARGO_TARGET_DIR=${cargo_target}"
          ${CARGO} build ${cargo_flags} -p purple-mimi-ffi
  WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
  DEPENDS ${rust_sources}
          "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml"
          "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.lock"
  COMMENT "Building purple-mimi-ffi (${cargo_dir})"
  VERBATIM)
add_custom_target(ffi DEPENDS "${ffi_lib}")

# What this staticlib needs from the system: rustls over aws-lc-rs and
# rusqlite's bundled SQLite want libm, pthreads and dl.
set(ffi_system_libs m dl pthread)

# --- the glues ---------------------------------------------------------

# Hides every symbol but the plugin's entry points, so two Rust plugins
# in one Pidgin do not resolve each other's (§10.1).
function(_hide_symbols target base)
  if(APPLE)
    target_link_options(${target} PRIVATE
                        "-Wl,-exported_symbols_list,${base}.syms")
    set_property(TARGET ${target} APPEND PROPERTY
                 LINK_DEPENDS "${base}.syms")
  else()
    target_link_options(${target} PRIVATE
                        "-Wl,--version-script,${base}.map"
                        "-Wl,--gc-sections")
    set_property(TARGET ${target} APPEND PROPERTY
                 LINK_DEPENDS "${base}.map")
  endif()
endfunction()

# sources: a ;-list, so a glue may be several translation units.
function(_add_glue target sources pc_target out_name exports_base plugin_dir)
  add_library(${target} MODULE ${sources})
  add_dependencies(${target} ffi)
  target_include_directories(${target} PRIVATE
                             "${CMAKE_CURRENT_SOURCE_DIR}/include")
  # GETTEXT_PACKAGE: glib/gi18n-lib.h insists on a domain, and this
  # plugin has its own rather than libpurple's. G_LOG_DOMAIN names us in
  # GLib's log output, which is how libpurple 3 protocols log.
  target_compile_definitions(${target} PRIVATE
                             "PURPLE_MIMI_VERSION=\"${PROJECT_VERSION}\""
                             "GETTEXT_PACKAGE=\"purple-mimi\""
                             "G_LOG_DOMAIN=\"purple-mimi\"")
  target_compile_options(${target} PRIVATE
                         -fvisibility=hidden -ffunction-sections
                         -fdata-sections -Wall -Wextra)
  target_link_libraries(${target} PRIVATE
                        "${ffi_lib}" ${pc_target} ${ffi_system_libs})
  # libpurple loads the file by name: no "lib" prefix of CMake's own.
  set_target_properties(${target} PROPERTIES
                        PREFIX "" OUTPUT_NAME "${out_name}" C_STANDARD 11)
  _hide_symbols(${target} "${exports_base}")
  install(TARGETS ${target} DESTINATION "${plugin_dir}")
endfunction()

if(PURPLE2)
  pkg_get_variable(PURPLE2_PLUGIN_DIR purple plugindir)
  if(NOT PURPLE2_PLUGIN_DIR)
    set(PURPLE2_PLUGIN_DIR "${CMAKE_INSTALL_PREFIX}/lib/purple-2")
  endif()
  _add_glue(purple-mimi2 "purple2/purple-mimi2.c" PkgConfig::PURPLE2_PC
            "libpurple-mimi"
            "${CMAKE_CURRENT_SOURCE_DIR}/purple2/exports"
            "${PURPLE2_PLUGIN_DIR}")
endif()

if(PURPLE3)
  pkg_get_variable(PURPLE3_PLUGIN_DIR purple-3 plugindir)
  if(NOT PURPLE3_PLUGIN_DIR)
    set(PURPLE3_PLUGIN_DIR "${CMAKE_INSTALL_PREFIX}/lib/purple-3")
  endif()
  _add_glue(purple-mimi3
            "purple3/purple-mimi3.c;purple3/mimiprotocol.c;purple3/mimiconnection.c"
            PkgConfig::PURPLE3_PC
            "libpurple3-mimi"
            "${CMAKE_CURRENT_SOURCE_DIR}/purple3/exports"
            "${PURPLE3_PLUGIN_DIR}")
endif()

message(STATUS "purple-mimi ${PROJECT_VERSION}")
message(STATUS "  libpurple 2 plugin: ${PURPLE2}")
message(STATUS "  libpurple 3 plugin: ${PURPLE3}")
message(STATUS "  cargo profile:      ${cargo_dir}")
