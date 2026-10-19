#ifndef DISPATCHARRTV_APP_H
#define DISPATCHARRTV_APP_H
#include <stdio.h>                 /* FILE */

/* Nothing from a local config file is compiled in: the binary carries no server address and no
 * credentials. The server and its API key come from the account stored at sign-in.
 *
 * The one process-wide global: the event/diagnostic log, DEFINED once as
 *     FILE *elogf = NULL; in main.c and read by nearly every UI/glue module. */
extern FILE *elogf;

#endif /* DISPATCHARRTV_APP_H */
