# follows a transient activation unit and prints one record per line
#   L x  a journal line of the unit
#   C x  the journal cursor after the lines so far
#   J x  the exit status of a journalctl that failed this round
#   W x  why systemctl could not read the unit this round
#   S x  the properties of the unit, the last record of a round
# the arguments are the unit, 1 to start it first, the cursor to resume after
# and, when starting, the activation command

unit=$1
start=$2
cursor=$3
shift 3

# journal lines are bytes
export LC_ALL=C

if [ "$start" = 1 ]; then
	systemd-run --unit="$unit" --service-type=exec --remain-after-exit -- "$@" || exit
fi

while :; do
	if ! state=$(systemctl show --property=LoadState,ActiveState,Result,ExecMainCode,ExecMainStatus "$unit" 2>&1); then
		printf 'W %s\n' "$(printf '%s' "$state" | tr '\n' ' ')"
		sleep 2
		continue
	fi
	state=$(printf '%s' "$state" | tr '\n' ' ')

	# the same test as ActivationState::parse
	case " $state " in
	*" LoadState=not-found "* | *" ActiveState=failed "*) finished=1 ;;
	*" ExecMainCode=0 "*) finished= ;;
	*) finished=1 ;;
	esac

	# journald may not have written the last lines yet
	if [ -n "$finished" ]; then
		journalctl --sync 2>/dev/null
	fi

	if lines=$(journalctl -u "$unit" -o cat --no-pager --quiet --show-cursor ${cursor:+"--after-cursor=$cursor"} 2>/dev/null); then
		# journalctl ends every batch of entries with the cursor after it
		footer=$(printf '%s\n' "$lines" | tail -n 1)
		case $footer in
		"-- cursor: "*)
			cursor=${footer#"-- cursor: "}
			printf '%s\n' "$lines" | sed -e '$d' -e 's/^/L /'
			;;
		esac
	else
		echo "J $?"
	fi

	if [ -n "$cursor" ]; then
		printf 'C %s\n' "$cursor"
	fi
	printf 'S %s\n' "$state"

	if [ -n "$finished" ]; then
		exit 0
	fi
	sleep 2
done
