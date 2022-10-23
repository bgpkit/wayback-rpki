# Wayback RPKI Updater

This project implements the crawler of RIPE RIS RPKI daily dump with a database
schema design to hold historical information accordingly.

## Deployment

Currently an updater is deployed on [fly.io](https://fly.io) with a
minimum VM size (`shared-cpu-1x` and `256MB Memory`).

The deployment requires setting `DATABASE_URL` environment variable.