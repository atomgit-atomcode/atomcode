#!/usr/bin/env bash
# Negative control: prove each gate can actually fail.
#
# A judge that only ever runs against sound objects has no discriminating power
# — an `exit 0` would pass just as well. Every check below feeds the gate a
# known-BAD object and requires it to say so.
set -uo pipefail
cd "$(dirname "$0")/.."

fail=0
must_detect() {                # must_detect <what> <test name that asserts the bad case is caught>
  local what="$1" test="$2"
  if cargo test -q -p atomcode-tui --lib "$test" 2>&1 | grep -q "1 passed"; then
    printf '  \033[32mok\033[0m   量具能判出：%s\n' "$what"
  else
    printf '  \033[31mFAIL\033[0m 量具判不出：%s（缺 %s）\n' "$what" "$test"
    fail=1
  fi
}

echo "阴性对照"
must_detect "模块画出自己的框"      containment_catches_a_module_drawing_outside_its_box
must_detect "终态块被改写"          a_settled_block_cannot_be_amended
must_detect "越权改别人的块"        another_producer_cannot_touch_my_block
must_detect "宽字符被拦腰截断"      a_wide_character_is_never_halved
must_detect "窄到放不下一个字时挂死" a_character_wider_than_the_line_is_dropped_not_hung_on
must_detect "未挂载的模块导致崩溃"   an_unmounted_module_collapses_instead_of_panicking
must_detect "两行争一个键"          two_rows_claiming_one_key_is_caught_at_mount_not_at_press
must_detect "两行争一个命令名"      two_rows_claiming_one_name_is_caught_at_mount
must_detect "命令的缝缺失时崩溃"    a_command_whose_seam_is_missing_says_so_instead_of_panicking
must_detect "markdown 遇到怪输入挂死" rendering_is_total_and_terminates_on_anything
must_detect "模态画出自己的框"      a_picker_never_draws_wider_than_its_rect
must_detect "第二个模态叠在第一个上" opening_a_second_modal_cancels_the_first_rather_than_stacking
must_detect "边框超出小矩形"        the_frame_fits_its_rect_at_any_size

# The instrument itself must be calibrated: a name that does not exist must FAIL
# this script's own check, or the script would pass with zero real coverage.
if cargo test -q -p atomcode-tui --lib no_such_test_exists_anywhere 2>&1 | grep -q "1 passed"; then
  printf '  \033[31mFAIL\033[0m 量具坏了：不存在的测试被判为通过\n'
  fail=1
else
  printf '  \033[32mok\033[0m   量具自身校准：不存在的测试判红\n'
fi
exit $fail
