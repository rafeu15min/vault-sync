#!/usr/bin/env python3
import os
import re
import json
import struct
import urllib.request

def get_steam_root():
    """Encontra o diretório principal da Steam no Linux."""
    paths_to_check = [
        os.path.expanduser("~/.local/share/Steam"),
        os.path.expanduser("~/.steam/steam"),
        os.path.expanduser("~/.steam/root")
    ]
    for path in paths_to_check:
        if os.path.isdir(path):
            return path
    return None

def parse_library_folders(steam_root):
    """Lê o libraryfolders.vdf para encontrar todos os HDs/SSDs onde a Steam instala jogos."""
    vdf_path = os.path.join(steam_root, "steamapps", "libraryfolders.vdf")
    libraries = [steam_root] # A raiz sempre é uma biblioteca
    
    if not os.path.isfile(vdf_path):
        return libraries

    try:
        with open(vdf_path, 'r', encoding='utf-8') as f:
            content = f.read()
            # Busca todos os caminhos listados como "path" "..."
            matches = re.findall(r'"path"\s+"([^"]+)"', content)
            for match in matches:
                # O VDF pode escapar barras duplas, limpamos isso
                clean_path = match.replace('\\\\', '/')
                if clean_path not in libraries and os.path.isdir(clean_path):
                    libraries.append(clean_path)
    except Exception as e:
        print(f"[Aviso] Erro ao ler libraryfolders.vdf: {e}")
        
    return libraries

def get_official_game_name(library_path, appid):
    """Busca o nome do jogo oficial lendo o arquivo appmanifest correspondente."""
    manifest_path = os.path.join(library_path, "steamapps", f"appmanifest_{appid}.acf")
    if os.path.isfile(manifest_path):
        try:
            with open(manifest_path, 'r', encoding='utf-8') as f:
                content = f.read()
                # Extrai o nome do jogo
                match = re.search(r'"name"\s+"([^"]+)"', content, re.IGNORECASE)
                if match:
                    return match.group(1)
        except Exception:
            pass
    return None

def parse_shortcuts_vdf(steam_root):
    """Lê o arquivo binário shortcuts.vdf para extrair AppIDs falsos e Nomes (Jogos Piratas/Hydra)."""
    userdata_path = os.path.join(steam_root, "userdata")
    shortcuts_map = {} # appid (str) -> name
    
    if not os.path.isdir(userdata_path):
        return shortcuts_map

    # Varre todos os IDs de usuário na Steam
    for user_id in os.listdir(userdata_path):
        vdf_path = os.path.join(userdata_path, user_id, "config", "shortcuts.vdf")
        if not os.path.isfile(vdf_path):
            continue
            
        try:
            with open(vdf_path, 'rb') as f:
                data = f.read()
                
            # O VDF binário possui um formato específico. 
            # 0x02 = Int32 (AppID), 0x01 = String (AppName)
            # Vamos usar uma heurística baseada em Regex de bytes para extrair os pares.
            
            # Encontra todos os AppIDs
            appid_matches = re.finditer(b'\x02appid\x00(....)', data)
            # Encontra todos os AppNames
            appname_matches = re.finditer(b'\x01appname\x00([^\x00]+)\x00', data)
            
            appids = [struct.unpack('<I', m.group(1))[0] for m in appid_matches]
            appnames = [m.group(1).decode('utf-8', errors='ignore') for m in appname_matches]
            
            # Associa baseado na ordem (formato do Steam)
            for i in range(min(len(appids), len(appnames))):
                # A Steam converte o int32 assinado para uma string de 10 dígitos (unsigned/signed math)
                # Para garantir o match com o nome da pasta em compatdata, tratamos como unsigned int de 32 bits
                unsigned_appid = str(appids[i] & 0xFFFFFFFF)
                shortcuts_map[unsigned_appid] = appnames[i]
                
        except Exception as e:
            print(f"[Aviso] Erro ao analisar shortcuts.vdf do usuário {user_id}: {e}")
            
    return shortcuts_map

def investigate_orphan(lib_path, appid):
    """Tenta descobrir de qual jogo era esse prefixo abandonado ativando o modo CSI."""
    if appid == "0":
        return "Sistema (Prefixo Base do Proton / Steamworks)"

    # 1. Se for um ID curto, é um jogo oficial da Steam que foi desinstalado
    if len(appid) <= 8:
        try:
            url = f"https://store.steampowered.com/api/appdetails?appids={appid}"
            req = urllib.request.Request(url, headers={'User-Agent': 'Mozilla/5.0'})
            with urllib.request.urlopen(req, timeout=2) as response:
                data = json.loads(response.read().decode('utf-8'))
                if data and str(appid) in data and data[str(appid)].get('success'):
                    game_name = data[str(appid)]['data'].get('name', 'Desconhecido')
                    return f"Órfão (Desinstalado da Steam: {game_name})"
        except Exception:
            pass
        return f"Órfão (AppID Steam: {appid} - Pesquise no SteamDB.info)"
        
    # 2. Se for ID longo (10 dígitos), é Pirata/Non-Steam. Vamos caçar pistas avançadas.
    pfx_path = os.path.join(lib_path, "steamapps", "compatdata", appid, "pfx")
    users_path = os.path.join(pfx_path, "drive_c", "users", "steamuser")
    
    clues = set()
    
    # --- PASSO A: Caça nas Pastas de Save e AppData ---
    search_dirs = [
        os.path.join(users_path, "Documents"),
        os.path.join(users_path, "Documents", "My Games"),
        os.path.join(users_path, "AppData", "Local"),
        os.path.join(users_path, "AppData", "LocalLow"),
        os.path.join(users_path, "AppData", "Roaming"),
        os.path.join(users_path, "Saved Games"),
        os.path.join(pfx_path, "drive_c", "ProgramData")
    ]
    
    # Filtro cirúrgico de lixos do Windows e Launchers
    ignore_list = {
        'microsoft', 'temp', 'openvr', 'steam', 'cef', 'dxvk-cache', 'gstreamer', 'windows', 'epicgames',
        'music', 'downloads', 'pictures', 'videos', 'desktop', 'public', 'templates', 'appdata', 
        'application data', 'local settings', 'my pictures', 'my videos', 'my music', 'start menu', 
        'nethood', 'printhood', 'recent', 'sendto', 'cookies', 'history', 'favorites', 'logs', 'nvidia', 'amd',
        'epic games', 'epic games launcher', 'ubisoft', 'ea desktop', 'origin', 'gog.com', 'program data'
    }
    
    for search_dir in search_dirs:
        if os.path.isdir(search_dir):
            try:
                for item in os.listdir(search_dir):
                    if os.path.isdir(os.path.join(search_dir, item)):
                        if item.lower() not in ignore_list and not item.startswith('.'):
                            clues.add(item)
            except Exception:
                pass

    # --- PASSO B: Caça aos Atalhos (.lnk) na Área de Trabalho ---
    desktop_paths = [
        os.path.join(users_path, "Desktop"),
        os.path.join(pfx_path, "drive_c", "users", "Public", "Desktop")
    ]
    for dp in desktop_paths:
        if os.path.isdir(dp):
            try:
                for item in os.listdir(dp):
                    if item.endswith(".lnk") or item.endswith(".url"):
                        name = item.rsplit(".", 1)[0]
                        if name.lower() not in ignore_list:
                            clues.add(f"Atalho: {name}")
            except Exception:
                pass

    # --- PASSO C: Caça no Registro do Wine (O Pente Fino Regex) ---
    ignore_reg = {'wine', 'microsoft', 'valve', 'classes', 'clients', 'policies', 'google', 'vulkan', 
                  'gstreamer', 'bugsplat', 'macromedia', 'khronos', 'epic games', 'epic games inc',
                  'gog.com', 'ubisoft', 'ea', 'origin', 'electronic arts', 'rockstar games', 'cd projekt red'}
    
    # Vamos ler tanto o user.reg (HKEY_CURRENT_USER) quanto o system.reg (HKEY_LOCAL_MACHINE)
    for r_file in ["user.reg", "system.reg"]:
        reg_file = os.path.join(pfx_path, r_file)
        if os.path.isfile(reg_file):
            try:
                with open(reg_file, 'r', encoding='utf-8', errors='ignore') as f:
                    for line in f:
                        # Regex Mágica: Pega apenas o que tá entre os colchetes, ignorando o timestamp fora
                        match = re.search(r'^\[Software\\\\([^\]]+)\]', line, re.IGNORECASE)
                        if match:
                            path_inner = match.group(1)
                            parts = path_inner.split('\\\\')
                            
                            # Entrando no túnel de 32 bits
                            if parts[0].lower() == 'wow6432node':
                                parts = parts[1:]
                                
                            if len(parts) >= 1:
                                vendor = parts[0].strip()
                                if vendor and vendor.lower() not in ignore_reg:
                                    clues.add(vendor)
                                    if len(parts) >= 2:
                                        game_name = parts[1].strip()
                                        if game_name and game_name.lower() not in ignore_reg:
                                            clues.add(game_name)
            except Exception:
                pass
                
    if clues:
        # Filtra sujeiras em branco e pega as 5 melhores pistas
        valid_clues = [c for c in clues if c.strip() and len(c) > 2]
        if valid_clues:
            clues_str = " | ".join(valid_clues[:5])
            return f"Órfão Pirata (Pistas: {clues_str})"
        
    return "Órfão (Completamente Vazio / Lixo)"

def main():
    steam_root = get_steam_root()
    if not steam_root:
        print("Erro: Diretório da Steam não encontrado.")
        return

    print(">> Lendo configurações da Steam...")
    libraries = parse_library_folders(steam_root)
    
    # Injetando as bibliotecas extras manualmente
    extra_libs = ["/mnt/SSD2/SteamLibrary", "/mnt/HDD1/SteamLibrary"]
    for extra in extra_libs:
        if os.path.isdir(extra) and extra not in libraries:
            libraries.append(extra)

    shortcuts_map = parse_shortcuts_vdf(steam_root)
    
    # Novo dicionário: { "localizacao": { "appid": "nome do jogo" } }
    catalog = {} 

    print(f">> Encontradas {len(libraries)} bibliotecas de jogos (incluindo manuais).")
    
    official_count = 0
    pirate_count = 0
    orphan_count = 0
    
    for lib in libraries:
        compatdata_path = os.path.join(lib, "steamapps", "compatdata")
        if not os.path.isdir(compatdata_path):
            continue
            
        if lib not in catalog:
            catalog[lib] = {}
            
        print(f">> Analisando prefixos em: {compatdata_path}")
        for appid in os.listdir(compatdata_path):
            if not appid.isdigit():
                continue
                
            # Tenta identificar como jogo Oficial
            official_name = get_official_game_name(lib, appid)
            
            if official_name:
                catalog[lib][appid] = official_name
                official_count += 1
            else:
                # Se não tem manifesto, é Pirata/Non-Steam ou um prefixo órfão
                non_steam_name = shortcuts_map.get(appid)
                
                if non_steam_name:
                    catalog[lib][appid] = non_steam_name
                    pirate_count += 1
                else:
                    # Prefixo abandonado (ex: jogo desinstalado mas o Proton ficou)
                    catalog[lib][appid] = investigate_orphan(lib, appid)
                    orphan_count += 1

    # Exportando para JSON
    output_file = "steam_catalog.json"
    try:
        with open(output_file, 'w', encoding='utf-8') as f:
            json.dump(catalog, f, indent=4, ensure_ascii=False)
        print(f"\n[SUCESSO] Relatório gerado: {output_file}")
        
        # Resumo rápido no terminal
        print(f"\nResumo da Varredura:")
        print(f" - Oficiais encontrados: {official_count}")
        print(f" - Piratas/Hydra encontrados: {pirate_count}")
        print(f" - Prefixos Órfãos (Lixo): {orphan_count}")
        
    except Exception as e:
        print(f"Erro ao salvar JSON: {e}")

if __name__ == "__main__":
    main()