/* Drives the real SDL_SendKeyboardText from jni/SDL2/src/events/SDL_keyboard.c
 * with the event queue stubbed: every pushed SDL_TEXTINPUT is recorded and the
 * pieces are checked against what was sent. Exit 0 = the paste arrived whole. */
#include <stdio.h>
#include <string.h>
#include "SDL_events.h"

int SDL_SendKeyboardText(const char *text);
/* SDL_internal.h routes every public call inside SDL through the dynamic-API
 * jump table (the _REAL names); the two the function needs are stubbed here. */

static char got[4096];
static int events, shortest = 1 << 20, longest;
int SDL_PushEvent_REAL(SDL_Event *e) {
    if (e->type == SDL_TEXTINPUT) {
        int n = (int)strlen(e->text.text);
        events++;
        if (n < shortest) shortest = n;
        if (n > longest) longest = n;
        strcat(got, e->text.text);
    }
    return 1;
}
Uint8 SDL_EventState_REAL(Uint32 type, int state) { (void)type; (void)state; return SDL_ENABLE; }

static int fails;
static void reset(void) { got[0] = 0; events = 0; shortest = 1 << 20; longest = 0; }
static void expect(const char *what, int ok) { printf("%s %s\n", ok ? "ok  " : "FAIL", what); if (!ok) fails++; }

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    /* 1. a long paste with multi-byte characters and newlines, ending in a newline */
    char text[512] = "";
    for (int i = 0; i < 6; i++) strcat(text, "line żółć 日本語 ünd more words here\n");
    reset(); SDL_SendKeyboardText(text);
    expect("long paste arrives whole", strcmp(got, text) == 0);
    expect("in more than one event", events > 1);
    expect("every event fits the 31-byte payload", longest <= SDL_TEXTINPUTEVENT_TEXT_SIZE - 1);
    expect("no event is a lone byte (sdl2tk would read it as a key)", shortest >= 2);
    /* 2. a paste that begins with a tab is text, not a key */
    reset(); SDL_SendKeyboardText("\tindented line\n\tsecond");
    expect("leading tab paste arrives whole", strcmp(got, "\tindented line\n\tsecond") == 0);
    /* 3. a lone newline is the Enter key: not text */
    reset(); SDL_SendKeyboardText("\n");
    expect("a lone newline posts nothing", events == 0);
    /* 4. a single character still arrives as one event */
    reset(); SDL_SendKeyboardText("a");
    expect("one char, one event", events == 1 && strcmp(got, "a") == 0);
    /* 5. 32 bytes: the boundary must not leave one byte behind */
    char x[33]; memset(x, 'x', 32); x[32] = 0;
    reset(); SDL_SendKeyboardText(x);
    expect("32 bytes arrive whole", strcmp(got, x) == 0);
    expect("32 bytes: no lone trailing byte", shortest >= 2);
    /* 6. two-byte characters split only on character boundaries */
    char z[128] = ""; for (int i = 0; i < 40; i++) strcat(z, "ż");
    reset(); SDL_SendKeyboardText(z);
    expect("multi-byte text arrives whole", strcmp(got, z) == 0);
    printf("%d failure(s)\n", fails);
    return fails ? 1 : 0;
}
