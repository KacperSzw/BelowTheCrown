# Przykład kontekstu NPC

[Pełny JSON](npc-context-example.json) zawiera rzeczywiste wejście i odpowiedź
Jadwigi (106), polskiej kucharki ze scenariusza `castle`, z decyzji 21 w ocenie
z 12 października 2026. To kontrolowana scena, bez wewnętrznego rozumowania
modelu. W tej próbce Elżbieta prosiła o jedną miskę gotowej potrawki.

Tożsamość (`actorId`, `name`, `role`, `biography`, `personality`) wiąże silnik
w instrukcji prywatnego wątku Codexa. Model nie wybiera osoby, za którą działa.
Produkcja używa `gpt-6-luna`, `high`, standardowego tieru. Wątek NPC jest
kontynuowany po poprawnej odpowiedzi; zmiana gałęzi, tożsamości, wczytanie gry
lub anulowanie unieważnia dawny kontekst. Interpreter wypowiedzi gracza ma
osobny, świeży wątek.

Każda tura dostaje JSON z dwoma częściami:

- `world`: własne cele, relacje i myśli, postrzegane pomieszczenie, osoby,
  przedmioty i ich posiadacze, znana mapa, oferty, zobowiązania, własne
  wspomnienia oraz chronologia rozmowy podczas bieżącej wizyty.
- `execution`: powód namysłu, zasady czasu, własne parametry ciała i umiejętności,
  aktywne zajęcie i pozostały plan, wyniki działań i niepowodzenia, katalog
  wykonalnych przykładów, przepisy i warunkowe prognozy.

Biografia i osobowość nie są powtarzane w JSON tury. Dokładny zapis tej próbki
powstał przed dodaniem `execution.people` z liczbowym HP widocznych osób;
w bieżącej wersji ten parametr również należy do części wykonania.
Instrukcje są w [npc.pl.md](../content/worlds/castle/prompts/npc.pl.md).

`world.roomContext` zawiera własne spostrzeżenia: kto mówił, znanego adresata,
głośność, usłyszany tekst, czas i liczbę powtórzeń oraz dostrzeżone wejścia,
wyjścia i obrażenia. Limit to 32 grupy wypowiedzi, osiem takich zdarzeń i 16 KiB.
Oddzielnie dobierane są najwyżej 24 własne wspomnienia. Zamaskowany krzyk
pozostaje zamaskowany; nieznany głos nie ujawnia tożsamości. Techniczne wpisy
rozpoczęcia i wykonania nie stają się nowymi wspomnieniami NPC.

## Narzędzia i wynik decyzji

NPC ma tylko `recall`, z jednym argumentem `query` do 180 znaków. Nie ma
narzędzia `speak`, terminala, przeglądarki, dostępu do plików gry ani cudzych
umysłów. W tej próbce, tak jak w całej nowej ocenie, nie wywołał `recall`.
Interpreter gracza nie ma żadnych narzędzi.

Przykład argumentu, gdy NPC potrzebuje przypomnieć dawną obietnicę:

```json
{"query":"obietnica kolacja Elżbieta"}
```

To ilustracja formatu, nie wywołanie z tej sceny. Silnik zwraca tablicę do
24 własnych wspomnień, z polami `id`, `actorId`, `time`, `text`, `kind`,
`importance`, `sourceId`, `eventId`, `evidenceIds` i opcjonalnym `perceived`.
`id` to ID wspomnienia, a `eventId` to osobne ID zdarzenia. Pusta tablica oznacza
brak wyniku. Właściciela pamięci ustala scheduler z aktywnego wątku, gałęzi
i zadania; model nie podaje go w argumencie. Odpowiedź wraca do Codexa jako
tekst JSON w `contentItems` typu `inputText`, z `success: true` dla poprawnego
zapytania. Zwrócone ID mogą uzasadniać zmianę wspomnienia lub relacji.

Model zwraca całą decyzję w ścisłym JSON. `summary`, `goals`, `memories`,
`relationships`, `acceptPlots` i `acceptHeardProposals` są prywatną częścią
decyzji. `plan` zawiera fizyczne kroki, a `utterance` jedną wypowiedź lub `null`.
Nie istnieje osobny format odpowiedzi modelu dla każdego kroku.

Jadwiga rzeczywiście zwróciła takie pola działania i dialogu:

```json
{
  "plan": [
    {"kind":"work","recipe":"meal"},
    {"kind":"give","itemId":1013,"targetId":107,"quantity":1}
  ],
  "utterance": {
    "text":"Weź tę miskę, Elżbieto. Nie każ królowi czekać, aż zrobię następną.",
    "targetId":107,
    "volume":"normal",
    "plot":null,
    "afterSteps":0
  }
}
```

To fragment pełnej odpowiedzi dostępnej w JSON. `afterSteps: 0` oznacza słowa
przed działaniami; `1` oznaczałoby po pierwszym kroku. Adapter zamienił to na
kanoniczną kolejność `speak → work → give`, bez kolejnego wywołania modelu.
Łączny limit to sześć kroków, wliczając wypowiedź. Słowa nie przekazują miski.

## Co wykonał silnik

Silnik przyjął decyzję: `ok: true`, „Decyzja postaci została przyjęta.” To
przyjęcie zamiaru. Potem ponownie sprawdził warunki każdego kroku i wykonał:

1. Wypowiedź Jadwigi na czacie.
2. Przygotowanie dodatkowej porcji: jeden składnik mniej, nowa potrawka 1028.
3. Przekazanie jednej porcji gotowej potrawki 1013 Elżbiecie. Oryginalny stos
   Jadwigi zmalał z dwóch do jednej porcji; Elżbieta dostała nową część 1029.

W czacie pojawił się rzeczywisty skutek: „Jadwiga przekazuje Gulasz na kolację
×1 osobie Elżbieta.” Prywatne wpisy „Zaczynasz…” mają typ `activity_started`;
UI ich nie pokazuje na czacie i silnik nie zapisuje ich jako spostrzeżeń NPC.
Końcowe `execution.self.lastActionOutcome` ma `stage: resolved`, `ok: true`
i rzeczywisty zamiar `give`. Kolejne wejście pokaże zmieniony ekwipunek.

Ten plan nadal nie był idealny: kucharka mogła od razu oddać gotową porcję,
a najpierw ugotowała dodatkową. W całej ocenie wykorzystano 24/24 decyzje:
22 przyjęto, dwie odrzucono, a trzy przyjęte plany później zawiodły. Rozmowy
pozostawały w świecie i samotne postaci milczały, lecz wybór fizycznych działań
nadal bywa błędny: angielska kucharka nadmiernie gotowała, a jedna polska
służąca połączyła uprzejmą prośbę z niepasującym atakiem obezwładniającym.
Poprawny JSON i naturalne zdanie nie dowodzą poprawnego zamiaru.
