[ -f ~/.bashrc ] && . ~/.bashrc

if [ -z "$GELI_SESSION_ACTIVE" ] && [ ! -e /tmp/.geli-session-active ]; then
    export GELI_SESSION_ACTIVE=1
    : > /tmp/.geli-session-active 2>/dev/null

    # Autologin is baked into the image, so the getty can hand us a shell before this session's
    # cloud-init has written /etc/geli/session. Wait for it rather than racing it.
    cloud-init status --wait >/dev/null 2>&1

    [ -f /etc/geli/env ] && . /etc/geli/env

    if [ -f /etc/geli/session ]; then
        . /etc/geli/session
        # The project lives on 9p, so flush before cutting power. `poweroff -f` skips the wall
        # broadcast and the orderly-shutdown log, neither of which belongs on the user's screen.
        sync
        sudo poweroff -f
    else
        echo "[!] geli: no session script found; cloud-init may have failed."
        echo "[!] See /var/log/cloud-init-output.log. Dropping to a shell."
    fi
else
    # Nested login shell, e.g. an agent's Bash tool. Load the environment, own nothing.
    [ -f /etc/geli/env ] && . /etc/geli/env
fi
