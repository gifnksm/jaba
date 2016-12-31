GITLAB_HOST="localhost"
GITLAB_PORT="10080"
GITLAB_SSH_PORT="10022"

GITLAB_URL="http://${GITLAB_HOST}:${GITLAB_PORT}"
GITLAB_API_URL="${GITLAB_URL}/api/v3"

TEMPLATE_DIR="./template"
TARGET_DIR="./target"

template() {
    local TEMPLATE="${1}"
    local SOURCE=$(cat <<OUTER_EOF
cat <<INNER_EOF
$(cat ${TEMPLATE})
INNER_EOF
OUTER_EOF
          )
    eval "${SOURCE}"
}

fetch_session() {
    local LOGIN="${1}"
    local PASSWORD="${2}"
    curl -sSf "${GITLAB_API_URL}/session" --data "login=${LOGIN}&password=${PASSWORD}"
}

api_get() {
    local PRIVATE_TOKEN="${1}"
    local API_PATH="${2}"
    curl -sSf --header "PRIVATE-TOKEN: ${PRIVATE_TOKEN}" "${GITLAB_API_URL}/${API_PATH}"
}
