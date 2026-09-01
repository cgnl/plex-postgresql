# Correction du crash UUID au démarrage (Hot-patching en mémoire)

Ce document décrit l'implémentation mise en place pour résoudre le plantage au démarrage de Plex Media Server sous PostgreSQL (exception fatale `std::domain_error: Invalid uuid length`).

## Description du problème
Au démarrage, lors du chargement des agents ou des plug-ins, Plex Media Server appelle une fonction interne de validation d'UUID située à l'adresse virtuelle `0x104a6cc` (sur architecture `aarch64`).

Cette fonction effectue des vérifications strictes sur la structure de la chaîne (longueur de 36 caractères et présence de tirets aux indices 8, 13, 18 et 23). Si la chaîne passée est invalide ou vide (ce qui arrive pour certains plug-ins sans UUID sous PostgreSQL), elle lève directement une exception fatale C++ `std::domain_error` qui fait crasher le serveur.

## Détails de l'implémentation du correctif

Le correctif consiste à intercepter et court-circuiter le validateur d'UUID directement en mémoire au chargement de l'interposeur (shim).

### 1. Résolution de l'adresse en mémoire
Plex Media Server étant compilé en mode PIE (Position Independent Executable), son adresse de chargement en mémoire est randomisée (ASLR).
- Nous utilisons la fonction standard `dl_iterate_phdr` de la libc pour parcourir les en-têtes de segments chargés.
- Le premier élément retourné par l'itérateur (index 0) correspond systématiquement à l'exécutable principal. Nous lisons son adresse de chargement (`dlpi_addr`) pour calculer l'adresse absolue cible :
  $$\text{target\_addr} = \text{base\_addr} + \text{0x104a6cc}$$

### 2. Écriture du patch via `/proc/self/mem`
Modifier les permissions des pages mémoire de code avec `mprotect` (`PROT_READ | PROT_WRITE`) échoue sur certains noyaux Docker stricts en raison des règles W^X (Write XOR Execute).
- Pour contourner cette protection de manière robuste, nous ouvrons le descripteur spécial de processus `/proc/self/mem` en mode écriture.
- L'écriture directe via `/proc/self/mem` permet de modifier les instructions de nos propres pages de code en mémoire, même si elles sont actuellement marquées en lecture-seule (`r-xp`).

### 3. Substitution d'instructions (AArch64 / ARM64)
Nous remplaçons le début de la fonction `0x104a6cc` par trois instructions assembleur ARM64 simples :
```assembly
movz x0, #0      ; Efface le registre x0 (première partie de l'UUID) -> octets: 00 00 80 d2
movz x1, #0      ; Efface le registre x1 (seconde partie de l'UUID)  -> octets: 01 00 80 d2
ret              ; Retourne de la fonction                          -> octets: c0 03 5f d6
```
Cette modification neutralise la fonction de validation en lui faisant systématiquement retourner un UUID vide normalisé ("nil" UUID), empêchant ainsi la levée de l'exception fatale.

Le cache d'instructions du processeur est ensuite invalidé et vidé via la fonction standard `__clear_cache` pour que les nouvelles instructions soient prises en compte immédiatement par le CPU.

## Fichiers concernés
- **Interposeur Rust** : [runtime_linux.rs](file:///Users/simon/Sources/github.com/cgnl/plex-postgresql/rust/plex-pg-core/src/runtime_linux.rs) (implémente la fonction `patch_uuid_parser`).
- **Scripts d'entrée** : [standalone-entrypoint.sh](file:///Users/simon/Sources/github.com/cgnl/plex-postgresql/scripts/standalone-entrypoint.sh) et [docker-entrypoint.sh](file:///Users/simon/Sources/github.com/cgnl/plex-postgresql/scripts/docker-entrypoint.sh) (pour formater les UUID initiaux dans `Preferences.xml` et synchroniser la table `devices`).
