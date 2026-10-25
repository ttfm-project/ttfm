# Copyright (C) 2026 The TTFM Project Contributors
# See the CONTRIBUTORS file at the top-level directory of this distribution
# for a list of copyright holders.
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program. If not, see <https://www.gnu.org/licenses/>.

_ttfm_completion() {
    local comp_type="${COMP_TYPE:-9}"
    if [ "$_TTFM_LAST_LINE" = "$COMP_LINE" ] && [ "$_TTFM_LAST_POINT" = "$COMP_POINT" ]; then
        case "$comp_type" in
            63|33|64)
                case "$_TTFM_LAST_TYPE" in
                    63|33|64) _TTFM_LAST_OFFSET=$((_TTFM_LAST_OFFSET + 40)) ;;
                    *) _TTFM_LAST_OFFSET="$_TTFM_LAST_OFFSET" ;;
                esac
                ;;
            *)
                _TTFM_LAST_OFFSET=$((_TTFM_LAST_OFFSET + 40))
                ;;
        esac
    else
        _TTFM_LAST_LINE="$COMP_LINE"
        _TTFM_LAST_POINT="$COMP_POINT"
        _TTFM_LAST_OFFSET=0
    fi
    _TTFM_LAST_TYPE="$comp_type"
    local out status
    out="$(ttfm complete --shell bash --offset "$_TTFM_LAST_OFFSET" --point "$COMP_POINT" --wordbreaks "$COMP_WORDBREAKS" -- "$COMP_LINE" 2>/dev/null)"
    status=$?
    if [ "$status" -eq 2 ]; then
        compopt -o default -o bashdefault 2>/dev/null
        COMPREPLY=()
    else
        compopt +o default +o bashdefault 2>/dev/null
        if [ -n "$out" ]; then
            mapfile -t COMPREPLY <<< "$out"
        else
            COMPREPLY=()
        fi
    fi
}
complete -o default -o bashdefault -o nospace -F _ttfm_completion ttfm
