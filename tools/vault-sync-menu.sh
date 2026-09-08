#!/bin/sh
# vault-sync-menu.sh — menu rofi pro catálogo unificado do vault-sync
# (Steam+Heroic+pirata, local+FeuVault). Mesmo padrão já usado em
# ~/.config/waybar/scripts/minimized.sh: este script é dono do loop do
# rofi, o vault-sync só resolve dados e executa. Sem "-theme" de propósito
# — herda o tema padrão já configurado em ~/.config/rofi/config.rasi.
#
# Seleção de Proton (atalhos non-Steam): o bypass da Steam pro launch direto
# (ver main.rs, incidente 2026-08-29/30) tirou a Steam da decisão de qual
# Proton/prefixo usar, então `vault-sync` exige uma versão escolhida
# (`Config::proton_overrides`) antes de lançar um atalho non-Steam. Este
# script cuida da UX: primeiro lançamento de um jogo assim abre um segundo
# Rofi automaticamente (a escolha vira o padrão); depois disso, Alt+P na
# lista principal reabre o mesmo seletor a qualquer momento pra trocar —
# `-kb-custom-1` é o mecanismo padrão do rofi pra isso (sai com código 10
# em vez de 0, sem precisar de nenhuma tecla "escondida").

set -eu

pkill -x wlogout 2>/dev/null || true

is_non_steam_selection() {
    case "$1" in
        "[Steam (non-Steam)]"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Abre o segundo Rofi com as versões de Proton instaladas e grava a escolha
# como padrão pra `$1` (linha de catálogo). Devolve 1 (sem gravar nada) se o
# usuário cancelar — nunca lança o jogo sem uma escolha explícita.
pick_proton_for() {
    choice=$(vault-sync list-proton | rofi -dmenu -i -p "vault-sync — Proton para: $1")
    [ -n "$choice" ] || return 1
    vault-sync set-proton "$1" "$choice"
}

# `set -e` mataria o script inteiro assim que o rofi devolvesse um código
# != 0 (Esc = 1, Alt+P = 10) — desliga só ao redor desta chamada pra
# conseguir inspecionar `$?` antes de decidir o que fazer.
set +e
selection=$(vault-sync catalog --format=rofi | rofi -dmenu -i -p "vault-sync" \
    -kb-custom-1 "Alt+p" -mesg "Enter: jogar · Alt+P: trocar o Proton (só non-Steam)")
rofi_rc=$?
set -e

if [ -z "$selection" ]; then
    exit 0
fi

if is_non_steam_selection "$selection"; then
    if [ "$rofi_rc" -eq 10 ]; then
        # Alt+P: troca deliberada, mesmo já havendo um padrão salvo.
        pick_proton_for "$selection" || exit 0
    elif ! vault-sync get-proton "$selection" >/dev/null 2>&1; then
        # Primeiro lançamento deste jogo non-Steam: a escolha de agora vira
        # o padrão — sem isso, `menu-launch` recusaria lançar (ver main.rs).
        pick_proton_for "$selection" || exit 0
    fi
fi

vault-sync menu-launch "$selection"
